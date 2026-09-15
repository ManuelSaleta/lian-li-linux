use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MediaPublication {
    pub id: String,
    pub preparation: String,
    pub media: String,
    pub fingerprint: String,
}

pub(crate) struct Directory(pub File);

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "Media publication directory must belong to its account and exclude other writers"
        );
        Ok(Self(file))
    }
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
    pub fn child(&self, name: &str, create: bool) -> Result<Self> {
        let path = self.path().join(name);
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => self.0.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("Creating managed media directory"),
            }
        }
        Self::open(&path)
    }
}

impl MediaPublication {
    pub fn validate(&self) -> Result<()> {
        let generated = |name: &str, prefix: &str| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                (6..=32).contains(&suffix.len())
                    && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        };
        ensure!(
            self.id.len() == 32
                && self.id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && generated(
                    &self.preparation,
                    &format!(".lianli-migration-{}-", self.id)
                )
                && generated(&self.media, ".lianli-media-")
                && self.fingerprint.len() == 64
                && self
                    .fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Invalid media publication identity"
        );
        Ok(())
    }

    pub fn verify_staged(&self, root: &File) -> Result<()> {
        self.validate()?;
        let root = Directory(root.try_clone()?);
        let stage = root.child(&self.preparation, false)?;
        let source = stage.child(&self.media, false)?;
        ensure!(
            fingerprint_directory(&source)? == self.fingerprint,
            "Prepared media changed before publication"
        );
        Ok(())
    }

    pub fn publish(&self, root: &File) -> Result<()> {
        self.validate()?;
        let root = Directory(root.try_clone()?);
        let stage = root.child(&self.preparation, false)?;
        let media = root.child("media", true)?;
        let imports = media.child("imports", true)?;
        let source_path = stage.path().join(&self.media);
        let target_path = imports.path().join(&self.id);
        match fs::symlink_metadata(&target_path) {
            Ok(_) => {
                ensure!(
                    fs::symlink_metadata(&source_path)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                    "Managed media destination already exists. Neither directory was replaced"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.verify_staged(&root.0)?;
                check_capacity(
                    &imports,
                    &stage.child(&self.media, false)?,
                    8 * 1024 * 1024 * 1024,
                )?;
                crate::media_ownership::record(
                    &media,
                    &imports,
                    &stage.child(&self.media, false)?,
                    &self.id,
                    &self.fingerprint,
                )?;
                let source = std::ffi::CString::new(self.media.as_bytes())?;
                let target = std::ffi::CString::new(self.id.as_bytes())?;
                ensure!(
                    unsafe {
                        libc::renameat2(
                            stage.0.as_raw_fd(),
                            source.as_ptr(),
                            imports.0.as_raw_fd(),
                            target.as_ptr(),
                            libc::RENAME_NOREPLACE,
                        )
                    } == 0,
                    "Publishing managed media: {}",
                    std::io::Error::last_os_error()
                );
            }
            Err(error) => return Err(error).context("Checking managed media destination"),
        }
        let target = imports.child(&self.id, false)?;
        ensure!(
            fingerprint_directory(&target)? == self.fingerprint,
            "Published media identity changed"
        );
        stage.0.sync_all()?;
        imports.0.sync_all()?;
        Ok(())
    }

    pub fn verify_published(&self, root: &File) -> Result<()> {
        self.validate()?;
        let root = Directory(root.try_clone()?);
        let media = root
            .child("media", false)?
            .child("imports", false)?
            .child(&self.id, false)?;
        ensure!(
            fingerprint_directory(&media)? == self.fingerprint,
            "Committed media is missing or changed. Inspect the migration before starting hardware"
        );
        Ok(())
    }
}

pub(crate) fn fingerprint(path: &Path) -> Result<String> {
    fingerprint_directory(&Directory::open(path)?)
}

pub(crate) fn check_capacity(imports: &Directory, staged: &Directory, limit: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut objects = std::collections::HashSet::new();
    let mut entries = 0usize;
    let mut bytes = 0u64;
    let mut scan = |directory: &Directory| -> Result<()> {
        for entry in fs::read_dir(directory.path())? {
            entries += 1;
            ensure!(
                entries <= 65_536 && Instant::now() < deadline,
                "Managed media quota inspection exceeded its entry or time limit"
            );
            let metadata = fs::symlink_metadata(entry?.path())?;
            ensure!(metadata.is_file(), "Managed media quota inspection refuses links, nested directories and special files");
            if objects.insert((metadata.dev(), metadata.ino())) {
                bytes = bytes
                    .checked_add(metadata.len())
                    .context("Managed media size overflow")?;
                ensure!(bytes <= limit, "Managed media storage exceeds its 8 GiB quota. Remove unused imports before copying more media");
            }
        }
        ensure!(
            Instant::now() < deadline,
            "Managed media quota inspection exceeded five seconds"
        );
        Ok(())
    };
    for (index, entry) in fs::read_dir(imports.path())?.enumerate() {
        ensure!(
            index < 1023 && Instant::now() < deadline,
            "Managed media quota inspection exceeds 1024 imports or five seconds"
        );
        scan(&Directory::open(&entry?.path())?)?;
    }
    scan(staged)
}

pub(crate) fn fingerprint_directory(directory: &Directory) -> Result<String> {
    let before = directory.0.metadata()?;
    let mut entries = BTreeMap::new();
    let mut objects = BTreeMap::new();
    let mut total = 0u64;
    for (index, entry) in fs::read_dir(directory.path())?.enumerate() {
        ensure!(
            index < 8192,
            "Managed media has more than 8192 directory entries"
        );
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid managed media filename"))?;
        let (digest, extension) = name
            .split_once('.')
            .map_or((name.as_str(), None), |(name, extension)| {
                (name, Some(extension))
            });
        ensure!(
            digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                && extension.is_none_or(|value| !value.is_empty()
                    && value.len() <= 16
                    && value.bytes().all(|byte| byte.is_ascii_alphanumeric())),
            "Unexpected managed media entry"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.len() <= 2 * 1024 * 1024 * 1024,
            "Managed media must be private regular files within the per-file limit"
        );
        if extension.is_none() {
            ensure!(
                objects
                    .insert((metadata.dev(), metadata.ino()), name.clone())
                    .is_none(),
                "Duplicate managed media objects"
            );
            total += metadata.len();
            ensure!(
                objects.len() <= 4096 && total <= 8 * 1024 * 1024 * 1024,
                "Managed media exceeds its publication budget"
            );
        }
        entries.insert(name, metadata);
    }
    let mut digest = Sha256::new();
    digest.update(b"lianli-media-identity-v1\0");
    // Device numbers may change across reboot; inode and change times survive recovery.
    digest.update(before.ino().to_le_bytes());
    for (name, metadata) in entries {
        let key = (metadata.dev(), metadata.ino());
        let object = objects
            .get(&key)
            .context("Managed media alias has no owned object")?;
        ensure!(
            name.split('.').next() == Some(object.as_str()),
            "Managed media alias identifies another object"
        );
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        for value in [
            metadata.ino(),
            metadata.len(),
            metadata.nlink(),
            u64::from(metadata.uid()),
            u64::from(metadata.gid()),
            u64::from(metadata.mode()),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ] {
            digest.update(value.to_le_bytes());
        }
    }
    let after = directory.0.metadata()?;
    ensure!(
        (
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec()
        ) == (
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        ),
        "Managed media directory changed during inspection"
    );
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod quota_tests {
    use super::*;

    #[test]
    fn retained_imports_count_once_per_inode_and_failed_capacity_preserves_all_files() {
        let root = tempfile::tempdir().unwrap();
        let root = Directory::open(root.path()).unwrap();
        let imports = root.child("imports", true).unwrap();
        let old = imports.child("old", true).unwrap();
        fs::write(old.path().join("asset"), b"12345").unwrap();
        fs::hard_link(old.path().join("asset"), old.path().join("asset.png")).unwrap();
        let staged = root.child("staged", true).unwrap();
        fs::write(staged.path().join("new"), b"67890").unwrap();
        assert!(check_capacity(&imports, &staged, 10).is_ok());
        assert!(check_capacity(&imports, &staged, 9).is_err());
        assert_eq!(fs::read(old.path().join("asset")).unwrap(), b"12345");
        assert_eq!(fs::read(staged.path().join("new")).unwrap(), b"67890");
        std::os::unix::fs::symlink(old.path().join("asset"), staged.path().join("alias")).unwrap();
        assert!(check_capacity(&imports, &staged, 100).is_err());
    }
}
