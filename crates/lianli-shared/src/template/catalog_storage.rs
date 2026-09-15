use anyhow::{ensure, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_STORE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;
const MAX_DEPTH: usize = 32;
pub(super) const RECEIPT_NAME: &str = ".lianli-catalog.json";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    schema_version: u32,
    directory_device: u64,
    directory_inode: u64,
    template_id: String,
    pub(super) files: Vec<super::catalog::CatalogFile>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CatalogStorageEntry {
    pub directory: String,
    pub bytes: u64,
    pub ownership_verified: bool,
    pub issue: Option<String>,
    #[serde(default)]
    pub saved_references: Vec<String>,
    #[serde(default)]
    pub saved_references_checked: bool,
    #[serde(default)]
    pub saved_reference_count: u32,
    #[serde(default)]
    pub runtime_referenced: bool,
    #[serde(default)]
    pub runtime_references_checked: bool,
}

pub fn inspect_storage(config_dir: &Path) -> Result<Vec<CatalogStorageEntry>> {
    let root = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(config_dir.join("templates"))
    {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("opening catalog storage"),
    };
    let mut budget = ScanBudget {
        bytes: 0,
        entries: 0,
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: &|| false,
    };
    let mut result = Vec::new();
    for entry in fs::read_dir(format!("/proc/self/fd/{}", root.as_raw_fd()))? {
        budget.check()?;
        ensure!(
            result.len() < 1024,
            "Catalog inventory exceeds 1024 top-level entries"
        );
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Catalog storage name is not UTF-8"))?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(entry.path())
            .context("catalog inventory requires regular directories")?;
        budget.bytes = 0;
        budget.entries += 1;
        budget.scan(&directory, 1)?;
        let issue = if !name.starts_with("catalog-") {
            Some("Older or manually installed catalog directory. Automatic cleanup is unavailable because it has no supported ownership receipt. Existing templates can still use these files.".into())
        } else {
            verify_receipt(&directory, &name)
                .map(|_| ())
                .or_else(|error| {
                    super::catalog_recovery::verify_empty(config_dir, &root, &directory, &name)
                        .with_context(|| format!("{error:#}"))
                })
                .err()
                .map(|error| format!("{error:#}").chars().take(2048).collect())
        };
        result.push(CatalogStorageEntry {
            directory: name,
            bytes: budget.bytes,
            ownership_verified: issue.is_none(),
            issue,
            saved_references: Vec::new(),
            saved_references_checked: false,
            saved_reference_count: 0,
            runtime_referenced: false,
            runtime_references_checked: false,
        });
    }
    budget.check()?;
    result.sort_by(|a, b| a.directory.cmp(&b.directory));
    Ok(result)
}

pub(super) fn verify_receipt(directory: &File, name: &str) -> Result<Receipt> {
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        .join(RECEIPT_NAME);
    let receipt_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("ownership receipt is missing or unreadable")?;
    let metadata = receipt_file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 128 * 1024,
        "Ownership receipt must be a regular file of at most 128 KiB"
    );
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fd/{}", receipt_file.as_raw_fd()))?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 128 * 1024,
        "Ownership receipt grew beyond 128 KiB"
    );
    let receipt: Receipt = serde_json::from_slice(&bytes).context("invalid ownership receipt")?;
    let actual = directory.metadata()?;
    ensure!(actual.uid() == unsafe { libc::geteuid() } && metadata.uid() == actual.uid()
        && actual.mode() & 0o022 == 0 && metadata.mode() & 0o022 == 0,
        "Catalog directory and receipt must be owned by the daemon account and not writable by other accounts");
    ensure!(
        receipt.schema_version == 1
            && receipt.directory_device == actual.dev()
            && receipt.directory_inode == actual.ino(),
        "Ownership receipt does not match this directory or schema"
    );
    super::catalog::validate_relative_path(&receipt.template_id)?;
    ensure!(
        !receipt.template_id.contains('/') && receipt.template_id.len() <= 128,
        "Invalid receipt template ID"
    );
    let prefix = format!("catalog-{}-", receipt.template_id);
    ensure!(
        name.strip_prefix(&prefix)
            .is_some_and(|suffix| suffix.len() == 6
                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())),
        "Ownership receipt does not match the generated directory name"
    );
    ensure!(
        !receipt.files.is_empty() && receipt.files.len() <= 129,
        "Invalid receipt file count"
    );
    let mut paths = std::collections::HashSet::new();
    for file in &receipt.files {
        super::catalog::validate_relative_path(&file.path)?;
        ensure!(
            file.path.split('/').next() != Some(RECEIPT_NAME) && paths.insert(&file.path),
            "Invalid or duplicate receipt path"
        );
        ensure!(
            file.sha256.len() == 64 && file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid receipt SHA-256"
        );
    }
    Ok(receipt)
}

pub(super) fn write_receipt(
    path: &Path,
    template: &super::catalog::CatalogTemplate,
) -> Result<usize> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = directory.metadata()?;
    let receipt = Receipt {
        schema_version: 1,
        directory_device: metadata.dev(),
        directory_inode: metadata.ino(),
        template_id: template.id.clone(),
        files: std::iter::once(super::catalog::CatalogFile {
            path: template.template_file.clone(),
            sha256: template.template_sha256.clone(),
        })
        .chain(template.files.iter().cloned())
        .collect(),
    };
    let bytes = serde_json::to_vec(&receipt)?;
    ensure!(
        bytes.len() <= 128 * 1024,
        "Catalog ownership receipt exceeds 128 KiB"
    );
    let pinned = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(pinned.join(RECEIPT_NAME))
        .context("creating catalog ownership receipt")?;
    file.write_all(&bytes)
        .context("writing catalog ownership receipt")?;
    file.sync_all()
        .context("syncing catalog ownership receipt")?;
    directory
        .sync_all()
        .context("syncing catalog ownership directory")?;
    Ok(bytes.len())
}

pub(super) fn check_capacity(
    root: &File,
    reservation: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<()> {
    let mut budget = ScanBudget {
        bytes: 0,
        entries: 0,
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled,
    };
    budget.scan(root, 0)?;
    budget.check()?;
    ensure!(budget.bytes.saturating_add(reservation) <= MAX_STORE_BYTES,
        "Catalog storage is at its 8 GiB limit ({} bytes retained). Free unreferenced catalog assets before installing. Each install reserves up to 256 MiB", budget.bytes);
    Ok(())
}

struct ScanBudget<'a, F> {
    bytes: u64,
    entries: usize,
    deadline: Instant,
    cancelled: &'a F,
}

impl<F: Fn() -> bool> ScanBudget<'_, F> {
    fn check(&self) -> Result<()> {
        ensure!(
            self.entries <= MAX_ENTRIES,
            "Catalog storage exceeds the entry inspection limit"
        );
        ensure!(
            !(self.cancelled)(),
            "Catalog storage inspection cancelled during shutdown"
        );
        ensure!(
            Instant::now() < self.deadline,
            "Catalog storage inspection exceeded five seconds. Check state storage before retrying"
        );
        Ok(())
    }

    fn scan(&mut self, directory: &File, depth: usize) -> Result<()> {
        self.check()?;
        ensure!(
            depth <= MAX_DEPTH,
            "Catalog storage exceeds the 32-directory depth limit"
        );
        let path = format!("/proc/self/fd/{}", directory.as_raw_fd());
        for entry in fs::read_dir(path).context("reading catalog storage usage")? {
            self.check()?;
            self.entries += 1;
            ensure!(self.entries <= MAX_ENTRIES, "Catalog storage exceeds the 16384-entry inspection limit. Clean up unreferenced assets before retrying");
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                let child = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(entry.path())
                    .context("opening catalog subdirectory")?;
                self.scan(&child, depth + 1)?;
            } else {
                ensure!(metadata.is_file(), "Catalog storage contains a symlink or special file. Repair the catalog directory before installing");
                self.bytes = self.bytes.saturating_add(metadata.len());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_distinguishes_ownership_from_legacy_and_replaced_receipts() {
        let root = tempfile::tempdir().unwrap();
        assert!(inspect_storage(root.path()).unwrap().is_empty());
        let storage = root.path().join("templates");
        fs::create_dir(&storage).unwrap();
        let owned = storage.join("catalog-cooler-ABC123");
        let legacy = storage.join("legacy");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&legacy).unwrap();
        let manifest: super::super::catalog::CatalogManifest =
            serde_json::from_str(include_str!("../../../../templates/default_templates.json"))
                .unwrap();
        let bytes = write_receipt(&owned, &manifest.templates[0]).unwrap();
        fs::write(owned.join("partial"), b"partial").unwrap();
        let entries = inspect_storage(root.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].ownership_verified);
        assert_eq!(entries[0].bytes, bytes as u64 + 7);
        assert!(!entries[1].ownership_verified);
        assert!(entries[1]
            .issue
            .as_ref()
            .unwrap()
            .contains("Older or manually installed"));
        assert!(!entries[1]
            .issue
            .as_ref()
            .unwrap()
            .contains("Invalid removal"));
        fs::copy(owned.join(RECEIPT_NAME), legacy.join(RECEIPT_NAME)).unwrap();
        assert!(!inspect_storage(root.path()).unwrap()[1].ownership_verified);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!inspect_storage(root.path()).unwrap()[0].ownership_verified);
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(owned.join(RECEIPT_NAME), b"invalid").unwrap();
        assert!(!inspect_storage(root.path()).unwrap()[0].ownership_verified);
        assert_eq!(fs::read(owned.join("partial")).unwrap(), b"partial");
    }

    #[test]
    fn inventory_can_measure_over_quota_directories_but_rejects_symlink_trees() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("templates/legacy");
        fs::create_dir_all(&path).unwrap();
        File::create(path.join("large"))
            .unwrap()
            .set_len(MAX_STORE_BYTES + 1)
            .unwrap();
        assert_eq!(
            inspect_storage(root.path()).unwrap()[0].bytes,
            MAX_STORE_BYTES + 1
        );
        std::os::unix::fs::symlink(root.path(), path.join("link")).unwrap();
        assert!(inspect_storage(root.path()).is_err());
    }

    #[test]
    fn receipt_records_directory_identity_and_expected_files_without_replacing_existing_data() {
        let root = tempfile::tempdir().unwrap();
        let manifest: super::super::catalog::CatalogManifest =
            serde_json::from_str(include_str!("../../../../templates/default_templates.json"))
                .unwrap();
        let template = &manifest.templates[0];
        let written = write_receipt(root.path(), template).unwrap();
        let path = root.path().join(RECEIPT_NAME);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(written, bytes.len());
        let receipt: Receipt = serde_json::from_slice(&bytes).unwrap();
        let metadata = fs::metadata(root.path()).unwrap();
        assert_eq!(receipt.schema_version, 1);
        assert_eq!(receipt.directory_device, metadata.dev());
        assert_eq!(receipt.directory_inode, metadata.ino());
        assert_eq!(receipt.template_id, template.id);
        assert_eq!(receipt.files.len(), template.files.len() + 1);
        assert_eq!(receipt.files[0].path, template.template_file);
        assert_eq!(receipt.files[0].sha256, template.template_sha256);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(write_receipt(root.path(), template).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn capacity_counts_nested_and_incomplete_assets_without_modifying_them() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("partial")).unwrap();
        let path = root.path().join("partial/asset");
        File::create(&path)
            .unwrap()
            .set_len(MAX_STORE_BYTES - 1024)
            .unwrap();
        let directory = File::open(root.path()).unwrap();
        check_capacity(&directory, 1024, &|| false).unwrap();
        assert!(check_capacity(&directory, 1025, &|| false).is_err());
        assert_eq!(fs::metadata(path).unwrap().len(), MAX_STORE_BYTES - 1024);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn inspection_rejects_links_cancellation_and_excessive_depth() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let directory = File::open(root.path()).unwrap();
        assert!(check_capacity(&directory, 0, &|| false).is_err());
        fs::remove_file(root.path().join("link")).unwrap();
        fs::write(root.path().join("file"), b"unchanged").unwrap();
        assert!(check_capacity(&directory, 0, &|| true).is_err());
        let mut nested = root.path().to_path_buf();
        for _ in 0..=MAX_DEPTH {
            nested.push("d");
            fs::create_dir(&nested).unwrap();
        }
        assert!(check_capacity(&directory, 0, &|| false).is_err());
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"unchanged");
    }

    #[test]
    fn inspection_stops_at_entry_and_time_budgets() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"x").unwrap();
        let directory = File::open(root.path()).unwrap();
        let mut budget = ScanBudget {
            bytes: 0,
            entries: MAX_ENTRIES,
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: &|| false,
        };
        assert!(budget.scan(&directory, 0).is_err());
        budget.entries = 0;
        budget.deadline = Instant::now();
        assert!(budget.scan(&directory, 0).is_err());
        assert_eq!(budget.entries, 0);
    }
}
