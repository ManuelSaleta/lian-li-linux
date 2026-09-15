use super::catalog_storage::{verify_receipt, RECEIPT_NAME};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogReviewedFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub matches_catalog: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogDirectoryReview {
    pub directory: String,
    pub sha256: String,
    pub bytes: u64,
    pub files: Vec<CatalogReviewedFile>,
    pub missing_files: Vec<String>,
}

#[derive(PartialEq, Eq, Serialize)]
struct Identity {
    device: u64,
    inode: u64,
    bytes: u64,
    uid: u32,
    mode: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn identity(metadata: &Metadata) -> Identity {
    Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        uid: metadata.uid(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

fn pinned(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
fn directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("opening catalog review directory")
}

/// This content snapshot does not establish whether assets are still referenced.
pub fn review_catalog_directory(config_dir: &Path, name: &str) -> Result<CatalogDirectoryReview> {
    super::catalog::validate_relative_path(name)?;
    ensure!(
        !name.contains('/'),
        "Catalog review requires one directory name"
    );
    let root = directory(&config_dir.join("templates"))?;
    let selected = directory(&pinned(&root).join(name))?;
    Ok(review_pinned(config_dir, &root, &selected, name)?.0)
}

fn review_pinned(
    config_dir: &Path,
    root: &File,
    selected: &File,
    name: &str,
) -> Result<(CatalogDirectoryReview, BTreeMap<String, Identity>)> {
    let expected: BTreeMap<_, _> = match verify_receipt(selected, name) {
        Ok(receipt) => receipt
            .files
            .into_iter()
            .map(|file| (file.path, file.sha256))
            .collect(),
        Err(error) => {
            super::catalog_recovery::verify_empty(config_dir, root, selected, name)
                .with_context(|| format!("{error:#}"))?;
            BTreeMap::new()
        }
    };
    let mut directories = HashSet::new();
    for path in expected.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            directories.insert(path.to_string_lossy().into_owned());
            parent = path.parent();
        }
    }
    let mut scan = ReviewScan {
        expected,
        directories,
        identities: BTreeMap::new(),
        files: BTreeMap::new(),
        bytes: 0,
        deadline: Instant::now() + Duration::from_secs(5),
    };
    scan.walk(selected, "", 0)?;
    ensure!(
        identity(&selected.metadata()?)
            == identity(&fs::symlink_metadata(pinned(root).join(name))?),
        "Catalog directory changed during review"
    );
    let missing_files = scan
        .expected
        .keys()
        .filter(|path| !scan.files.contains_key(*path))
        .cloned()
        .collect();
    let files: Vec<_> = scan.files.into_values().collect();
    let sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(name, &scan.identities, &files))?)
    );
    Ok((
        CatalogDirectoryReview {
            directory: name.into(),
            sha256,
            bytes: scan.bytes,
            files,
            missing_files,
        },
        scan.identities,
    ))
}

/// The caller must exclude new references and validate saved/runtime references in `authorize`.
pub fn remove_catalog_directory(
    config_dir: &Path,
    name: &str,
    sha256: &str,
    authorize: impl FnOnce() -> Result<()>,
) -> Result<()> {
    super::catalog::validate_relative_path(name)?;
    ensure!(!name.contains('/'), "Select one catalog directory");
    let root = directory(&config_dir.join("templates"))?;
    let selected = directory(&pinned(&root).join(name))?;
    let (review, _) = review_pinned(config_dir, &root, &selected, name)?;
    ensure!(
        review.sha256 == sha256,
        "Catalog contents changed. Review again"
    );
    authorize()?;
    let (current, identities) = review_pinned(config_dir, &root, &selected, name)?;
    ensure!(
        current.sha256 == sha256,
        "Catalog contents changed. Review again"
    );
    super::catalog_recovery::prepare(config_dir, &root, &selected, name)?;
    remove_reviewed(
        &selected,
        "",
        &identities,
        Instant::now() + Duration::from_secs(5),
    )?;
    let current = fs::symlink_metadata(pinned(&root).join(name))?;
    let selected_metadata = selected.metadata()?;
    ensure!(
        current.dev() == selected_metadata.dev() && current.ino() == selected_metadata.ino(),
        "Catalog directory was replaced. Preserve the replacement"
    );
    fs::remove_dir(pinned(&root).join(name)).context("removing empty catalog directory")?;
    root.sync_all().context("syncing catalog removal")?;
    super::catalog_recovery::finish(config_dir, &root, &selected, name)
}

fn remove_reviewed(
    handle: &File,
    relative: &str,
    identities: &BTreeMap<String, Identity>,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        identities.get(relative) == Some(&identity(&handle.metadata()?)),
        "Catalog directory changed before removal. Review remaining files again"
    );
    let mut children: Vec<_> = identities
        .keys()
        .filter(|path| !path.is_empty() && Path::new(path).parent() == Some(Path::new(relative)))
        .collect();
    // Keep ownership evidence until all asset removals have completed.
    children.sort_by_key(|path| path.as_str() == RECEIPT_NAME);
    for path in children {
        ensure!(
            Instant::now() < deadline,
            "Catalog removal timed out. Review remaining files again"
        );
        let name = Path::new(path)
            .file_name()
            .context("missing reviewed file name")?;
        if path.as_str() == RECEIPT_NAME {
            let remaining = fs::read_dir(pinned(handle))?
                .take(2)
                .collect::<std::io::Result<Vec<_>>>()?;
            ensure!(
                remaining.len() == 1 && remaining[0].file_name() == RECEIPT_NAME,
                "Unexpected catalog contents remain. Preserve ownership receipt and review again"
            );
            handle
                .sync_all()
                .context("syncing catalog asset removal before receipt removal")?;
        }
        let target = pinned(handle).join(name);
        let metadata = fs::symlink_metadata(&target)?;
        ensure!(
            identities.get(path) == Some(&identity(&metadata)),
            "Catalog file changed before removal. Review remaining files again"
        );
        if metadata.is_dir() {
            let child = directory(&target)?;
            remove_reviewed(&child, path, identities, deadline)?;
            let current = fs::symlink_metadata(&target)?;
            ensure!(
                current.dev() == metadata.dev() && current.ino() == metadata.ino(),
                "Catalog child directory was replaced. Preserve the replacement"
            );
            fs::remove_dir(&target)?;
        } else {
            fs::remove_file(&target)?;
        }
    }
    handle
        .sync_all()
        .context("syncing removed catalog contents")
}

struct ReviewScan {
    expected: BTreeMap<String, String>,
    directories: HashSet<String>,
    identities: BTreeMap<String, Identity>,
    files: BTreeMap<String, CatalogReviewedFile>,
    bytes: u64,
    deadline: Instant,
}

impl ReviewScan {
    fn check(&self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "Catalog content review exceeded five seconds"
        );
        ensure!(
            self.identities.len() <= 8192 && self.files.len() <= 130,
            "Catalog review exceeds file/directory limits"
        );
        Ok(())
    }

    fn walk(&mut self, handle: &File, relative: &str, depth: usize) -> Result<()> {
        self.check()?;
        ensure!(depth <= 32, "Catalog review exceeds 32 directory levels");
        let before = identity(&handle.metadata()?);
        ensure!(
            before.uid == unsafe { libc::geteuid() } && before.mode & 0o022 == 0,
            "Catalog review found an insecure or foreign-owned directory"
        );
        for entry in fs::read_dir(pinned(handle))? {
            self.check()?;
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Catalog file name is not UTF-8"))?;
            let path = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(entry.path())?;
            let metadata = file.metadata()?;
            if metadata.is_dir() {
                ensure!(
                    self.directories.contains(&path),
                    "Unexpected catalog directory '{path}'. Preserve it for manual review"
                );
                let child = File::open(pinned(&file))?;
                self.walk(&child, &path, depth + 1)?;
            } else {
                ensure!(
                    metadata.is_file(),
                    "Catalog review refuses symlinks and special files"
                );
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
                    "Catalog review found an insecure or foreign-owned file"
                );
                ensure!(
                    path == RECEIPT_NAME || self.expected.contains_key(&path),
                    "Unexpected catalog file '{path}'. Preserve it for manual review"
                );
                ensure!(
                    metadata.len() <= MAX_BYTES - self.bytes,
                    "Catalog review exceeds 256 MiB"
                );
                let before = identity(&metadata);
                let mut reader = File::open(pinned(&file))?;
                let mut hasher = Sha256::new();
                let mut buffer = [0; 64 * 1024];
                let mut bytes = 0;
                loop {
                    self.check()?;
                    let count = reader.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    self.bytes += count as u64;
                    bytes += count as u64;
                    ensure!(
                        self.bytes <= MAX_BYTES,
                        "Catalog files grew beyond 256 MiB during review"
                    );
                    hasher.update(&buffer[..count]);
                }
                ensure!(
                    before == identity(&file.metadata()?),
                    "Catalog file changed during review"
                );
                let sha256 = format!("{:x}", hasher.finalize());
                let matches_catalog = self
                    .expected
                    .get(&path)
                    .map(|expected| sha256.eq_ignore_ascii_case(expected));
                self.identities.insert(path.clone(), before);
                self.files.insert(
                    path.clone(),
                    CatalogReviewedFile {
                        path,
                        bytes,
                        sha256,
                        matches_catalog,
                    },
                );
            }
        }
        self.check()?;
        ensure!(
            before == identity(&handle.metadata()?),
            "Catalog directory contents changed during review"
        );
        self.identities.insert(relative.into(), before);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        super::super::catalog::CatalogTemplate,
    ) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("templates/catalog-cooler-ABC123");
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let manifest: super::super::catalog::CatalogManifest =
            serde_json::from_str(include_str!("../../../../templates/default_templates.json"))
                .unwrap();
        let mut template = manifest.templates[0].clone();
        template.template_sha256 = format!("{:x}", Sha256::digest(b"original"));
        super::super::catalog_storage::write_receipt(&path, &template).unwrap();
        fs::write(path.join(&template.template_file), b"original").unwrap();
        fs::set_permissions(
            path.join(&template.template_file),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        (root, path, template)
    }

    #[test]
    fn review_binds_contents_and_identity_and_reports_partial_or_changed_assets() {
        let (root, path, template) = fixture();
        let partial = path.join(&template.files[0].path);
        fs::write(&partial, b"partial").unwrap();
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o644)).unwrap();
        let first = review_catalog_directory(root.path(), "catalog-cooler-ABC123").unwrap();
        assert_eq!(first.missing_files, vec![template.files[1].path.clone()]);
        assert!(first
            .files
            .iter()
            .any(|file| file.path == template.template_file && file.matches_catalog == Some(true)));
        assert!(
            first
                .files
                .iter()
                .any(|file| file.path == template.files[0].path
                    && file.matches_catalog == Some(false))
        );
        assert_eq!(
            first.sha256,
            review_catalog_directory(root.path(), "catalog-cooler-ABC123")
                .unwrap()
                .sha256
        );
        fs::write(&partial, b"changed").unwrap();
        assert_ne!(
            first.sha256,
            review_catalog_directory(root.path(), "catalog-cooler-ABC123")
                .unwrap()
                .sha256
        );
        assert_eq!(fs::read(partial).unwrap(), b"changed");
    }

    #[test]
    fn review_preserves_unexpected_files_and_rejects_links_traversal_and_oversized_assets() {
        let (root, path, template) = fixture();
        let extra = path.join("personal.txt");
        fs::write(&extra, b"preserve").unwrap();
        assert!(review_catalog_directory(root.path(), "catalog-cooler-ABC123").is_err());
        assert_eq!(fs::read(&extra).unwrap(), b"preserve");
        fs::remove_file(&extra).unwrap();
        let asset = path.join(&template.files[0].path);
        std::os::unix::fs::symlink(root.path(), &asset).unwrap();
        assert!(review_catalog_directory(root.path(), "catalog-cooler-ABC123").is_err());
        fs::remove_file(&asset).unwrap();
        File::create(&asset)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        fs::set_permissions(&asset, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(review_catalog_directory(root.path(), "catalog-cooler-ABC123").is_err());
        assert!(review_catalog_directory(root.path(), "../catalog-cooler-ABC123").is_err());
        assert_eq!(fs::metadata(asset).unwrap().len(), MAX_BYTES + 1);
    }

    #[test]
    fn removal_rechecks_authorization_and_contents_before_deleting_partial_install() {
        let (root, path, template) = fixture();
        let review = review_catalog_directory(root.path(), "catalog-cooler-ABC123").unwrap();
        assert!(remove_catalog_directory(
            root.path(),
            &review.directory,
            &review.sha256,
            || anyhow::bail!("now referenced")
        )
        .is_err());
        assert_eq!(
            fs::read(path.join(&template.template_file)).unwrap(),
            b"original"
        );
        assert!(
            remove_catalog_directory(root.path(), &review.directory, &review.sha256, || {
                fs::write(path.join("personal.txt"), b"preserve")?;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(path.join("personal.txt")).unwrap(), b"preserve");
        assert!(path.join(RECEIPT_NAME).exists());
        fs::remove_file(path.join("personal.txt")).unwrap();
        assert!(remove_catalog_directory(
            root.path(),
            &review.directory,
            &review.sha256,
            || Ok(())
        )
        .is_err());
        let review = review_catalog_directory(root.path(), &review.directory).unwrap();
        remove_catalog_directory(root.path(), &review.directory, &review.sha256, || Ok(()))
            .unwrap();
        assert!(!path.exists());
        assert!(root.path().join("templates").is_dir());
    }

    #[test]
    fn removal_handles_reviewed_nested_assets_without_touching_siblings() {
        let (root, path, mut template) = fixture();
        fs::remove_file(path.join(RECEIPT_NAME)).unwrap();
        template.files[0].path = "nested/asset.png".into();
        fs::create_dir(path.join("nested")).unwrap();
        fs::set_permissions(path.join("nested"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(path.join("nested/asset.png"), b"partial").unwrap();
        fs::set_permissions(
            path.join("nested/asset.png"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        super::super::catalog_storage::write_receipt(&path, &template).unwrap();
        let sibling = root.path().join("templates/personal");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("keep"), b"preserve").unwrap();
        let review = review_catalog_directory(root.path(), "catalog-cooler-ABC123").unwrap();
        remove_catalog_directory(root.path(), &review.directory, &review.sha256, || Ok(()))
            .unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(sibling.join("keep")).unwrap(), b"preserve");
    }

    #[test]
    fn removal_preserves_original_and_replacement_when_directory_is_swapped() {
        let (root, path, template) = fixture();
        let review = review_catalog_directory(root.path(), "catalog-cooler-ABC123").unwrap();
        let moved = root.path().join("original");
        assert!(
            remove_catalog_directory(root.path(), &review.directory, &review.sha256, || {
                fs::rename(&path, &moved)?;
                fs::create_dir(&path)?;
                fs::write(path.join("personal.txt"), b"preserve")?;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(
            fs::read(moved.join(&template.template_file)).unwrap(),
            b"original"
        );
        assert!(moved.join(RECEIPT_NAME).exists());
        assert_eq!(fs::read(path.join("personal.txt")).unwrap(), b"preserve");
    }

    #[test]
    fn interrupted_receipt_removal_remains_reviewable_only_while_empty_and_identical() {
        let (root, path, template) = fixture();
        let name = "catalog-cooler-ABC123";
        let storage = directory(&root.path().join("templates")).unwrap();
        let selected = directory(&path).unwrap();
        super::super::catalog_recovery::prepare(root.path(), &storage, &selected, name).unwrap();
        fs::remove_file(path.join(&template.template_file)).unwrap();
        fs::remove_file(path.join(RECEIPT_NAME)).unwrap();
        let review = review_catalog_directory(root.path(), name).unwrap();
        assert!(review.files.is_empty());
        assert!(
            super::super::catalog_storage::inspect_storage(root.path()).unwrap()[0]
                .ownership_verified
        );
        fs::write(path.join("personal"), b"preserve").unwrap();
        assert!(review_catalog_directory(root.path(), name).is_err());
        assert!(remove_catalog_directory(root.path(), name, &review.sha256, || Ok(())).is_err());
        fs::remove_file(path.join("personal")).unwrap();
        let moved = root.path().join("moved");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(review_catalog_directory(root.path(), name).is_err());
        fs::remove_dir(&path).unwrap();
        fs::rename(moved, &path).unwrap();
        let review = review_catalog_directory(root.path(), name).unwrap();
        remove_catalog_directory(root.path(), name, &review.sha256, || Ok(())).unwrap();
        assert!(!path.exists());
        assert!(!root.path().join(".catalog-removal.json").exists());
    }

    #[test]
    fn completed_removal_marker_can_be_replaced_but_a_live_target_is_preserved() {
        let (root, path, _) = fixture();
        let storage = directory(&root.path().join("templates")).unwrap();
        let selected = directory(&path).unwrap();
        super::super::catalog_recovery::prepare(
            root.path(),
            &storage,
            &selected,
            "catalog-cooler-ABC123",
        )
        .unwrap();
        let other = root.path().join("templates/catalog-other-DEF456");
        fs::create_dir(&other).unwrap();
        let other_handle = directory(&other).unwrap();
        assert!(super::super::catalog_recovery::prepare(
            root.path(),
            &storage,
            &other_handle,
            "catalog-other-DEF456"
        )
        .is_err());
        fs::remove_dir_all(&path).unwrap();
        super::super::catalog_recovery::prepare(
            root.path(),
            &storage,
            &other_handle,
            "catalog-other-DEF456",
        )
        .unwrap();
        super::super::catalog_recovery::verify_empty(
            root.path(),
            &storage,
            &other_handle,
            "catalog-other-DEF456",
        )
        .unwrap();
    }
}
