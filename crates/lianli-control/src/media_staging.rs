use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_STAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ASSETS: usize = 4096;

pub struct CopyControl {
    cancelled: AtomicBool,
    copied: AtomicU64,
    deadline: Instant,
}

impl CopyControl {
    pub fn new(timeout: Duration) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            copied: AtomicU64::new(0),
            deadline: Instant::now() + timeout.min(Duration::from_secs(600)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn copied_bytes(&self) -> u64 {
        self.copied.load(Ordering::Relaxed)
    }

    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "Media staging was cancelled"
        );
        ensure!(Instant::now() < self.deadline, "Media staging timed out");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::symlink;

    #[test]
    fn copies_from_descriptors_deduplicates_content_and_preserves_extensions_and_offsets() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("source.png");
        let bytes = vec![17; 128 * 1024];
        fs::write(&source_path, &bytes).unwrap();
        let mut source = File::open(&source_path).unwrap();
        source.seek(SeekFrom::Start(7)).unwrap();
        let control = CopyControl::new(Duration::from_secs(5));
        let mut staging = MediaStaging::new(root.path(), Path::new("/destination/media")).unwrap();
        let first = staging
            .add(Path::new("/descriptor-only.PNG"), &source, &control)
            .unwrap();
        let second = staging
            .add(Path::new("/another-name.jpg"), &source, &control)
            .unwrap();
        let again = staging
            .add(Path::new("/descriptor-only.PNG"), &source, &control)
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(first.extension().unwrap(), "png");
        assert_eq!(second.extension().unwrap(), "jpg");
        let first_file = staging.directory().join(first.file_name().unwrap());
        let second_file = staging.directory().join(second.file_name().unwrap());
        assert_eq!(fs::read(&first_file).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&first_file).unwrap().ino(),
            fs::metadata(&second_file).unwrap().ino()
        );
        assert_eq!(fs::metadata(&first_file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(staging.unique_files(), 1);
        assert_eq!(staging.stored_bytes(), bytes.len() as u64);
        assert_eq!(control.copied_bytes(), 2 * bytes.len() as u64);
        assert_eq!(source.stream_position().unwrap(), 7);
        staging.sync().unwrap();
        drop(staging);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(fs::read(&source_path).unwrap(), bytes);
    }

    #[test]
    fn cancellation_deadlines_budgets_and_changed_sources_do_not_publish_extra_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.png");
        fs::write(&path, b"12345678").unwrap();
        let source = File::open(&path).unwrap();
        let mut staging = MediaStaging::new(root.path(), Path::new("/destination/media")).unwrap();
        staging.asset_limit = 8;
        staging.stage_limit = 8;
        let cancelled = CopyControl::new(Duration::from_secs(5));
        cancelled.cancel();
        assert!(staging
            .add(&path, &source, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(staging
            .add(&path, &source, &CopyControl::new(Duration::ZERO))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(staging.paths().is_empty());
        let control = CopyControl::new(Duration::from_secs(5));
        staging.add(&path, &source, &control).unwrap();
        assert!(staging
            .add(Path::new("/duplicate.png"), &source, &control)
            .unwrap_err()
            .to_string()
            .contains("budget"));
        fs::write(&path, b"changed").unwrap();
        assert!(staging
            .add(&path, &source, &control)
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert_eq!(staging.paths().len(), 1);
        let directory = staging.directory().file_name().unwrap().to_os_string();
        drop(staging);
        assert!(!root.path().join(directory).exists());
    }

    #[test]
    fn pinned_parent_survives_path_replacement_and_cleanup_does_not_follow_the_replacement() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let renamed = root.path().join("renamed");
        let sentinel = root.path().join("sentinel");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), b"untouched").unwrap();
        let input = root.path().join("input.png");
        fs::write(&input, b"asset").unwrap();
        let source = File::open(&input).unwrap();
        let mut staging = MediaStaging::new(&parent, Path::new("/destination/media")).unwrap();
        fs::rename(&parent, &renamed).unwrap();
        symlink(&sentinel, &parent).unwrap();
        let destination = staging
            .add(&input, &source, &CopyControl::new(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(
            fs::read(staging.directory().join(destination.file_name().unwrap())).unwrap(),
            b"asset"
        );
        drop(staging);
        assert_eq!(fs::read_dir(renamed).unwrap().count(), 0);
        assert_eq!(fs::read(sentinel.join("keep")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(sentinel).unwrap().count(), 1);
    }

    #[test]
    fn rejects_oversized_and_nonregular_sources_before_copying() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("too-large.mp4");
        let source = File::create(&path).unwrap();
        source.set_len(MAX_ASSET_BYTES + 1).unwrap();
        let control = CopyControl::new(Duration::from_secs(5));
        let mut staging = MediaStaging::new(root.path(), Path::new("/destination/media")).unwrap();
        assert!(staging
            .add(&path, &source, &control)
            .unwrap_err()
            .to_string()
            .contains("2 GiB"));
        assert!(staging
            .add(root.path(), &File::open(root.path()).unwrap(), &control)
            .unwrap_err()
            .to_string()
            .contains("not a regular media file"));
        assert_eq!(control.copied_bytes(), 0);
        assert_eq!(fs::read_dir(staging.directory()).unwrap().count(), 0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<&Metadata> for SourceIdentity {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

pub struct MediaStaging {
    // TempDir cleanup resolves through this descriptor, which must outlive the directory.
    directory: tempfile::TempDir,
    parent: File,
    destination: PathBuf,
    paths: HashMap<PathBuf, PathBuf>,
    sources: HashMap<PathBuf, SourceIdentity>,
    objects: HashMap<String, PathBuf>,
    stored_bytes: u64,
    asset_limit: u64,
    stage_limit: u64,
}

impl MediaStaging {
    pub fn new(parent: &Path, destination: &Path) -> Result<Self> {
        ensure!(
            destination.is_absolute() && destination.as_os_str().len() <= 4000,
            "Managed media destination must be an absolute directory path of at most 4000 bytes"
        );
        ensure!(
            !destination.as_os_str().as_encoded_bytes().contains(&0),
            "Managed media destination contains a NUL byte"
        );
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(parent)
            .context("Opening media staging parent")?;
        let metadata = parent.metadata()?;
        ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "Media staging parent must belong to the current account and must not be writable by other accounts");
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let directory = tempfile::Builder::new()
            .prefix(".lianli-media-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(anchored)?;
        Ok(Self {
            directory,
            parent,
            destination: destination.into(),
            paths: HashMap::new(),
            sources: HashMap::new(),
            objects: HashMap::new(),
            stored_bytes: 0,
            asset_limit: MAX_ASSET_BYTES,
            stage_limit: MAX_STAGE_BYTES,
        })
    }

    /// The descriptor must already be opened under the source account. This method
    /// never opens source_path. Run file I/O in a supervised worker: a filesystem
    /// syscall can remain blocked between cancellation checks.
    pub fn add(
        &mut self,
        source_path: &Path,
        source: &File,
        control: &CopyControl,
    ) -> Result<PathBuf> {
        control.check()?;
        ensure!(
            source_path.is_absolute() && source_path.as_os_str().len() <= 4096,
            "Source media path must be absolute and at most 4096 bytes"
        );
        ensure!(
            !source_path.as_os_str().as_encoded_bytes().contains(&0),
            "Source media path contains a NUL byte"
        );
        let metadata = source
            .metadata()
            .with_context(|| format!("Inspecting {}", source_path.display()))?;
        ensure!(
            metadata.is_file(),
            "{} is not a regular media file",
            source_path.display()
        );
        let identity = SourceIdentity::from(&metadata);
        if let Some(previous) = self.sources.get(source_path) {
            ensure!(
                *previous == identity,
                "{} changed during media staging",
                source_path.display()
            );
            return Ok(self.paths[source_path].clone());
        }
        ensure!(
            self.paths.len() < MAX_ASSETS,
            "Media staging exceeds 4096 source files"
        );
        ensure!(
            metadata.len() <= self.asset_limit,
            "{} exceeds the 2 GiB media-file limit",
            source_path.display()
        );
        ensure!(
            metadata.len() <= self.stage_limit.saturating_sub(self.stored_bytes),
            "Media staging exceeds its 8 GiB budget, including the temporary copy"
        );
        let mut temporary = tempfile::NamedTempFile::new_in(self.directory.path())?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut offset = 0;
        loop {
            control.check()?;
            let count = source
                .read_at(&mut buffer, offset)
                .with_context(|| format!("Reading {}", source_path.display()))?;
            if count == 0 {
                break;
            }
            ensure!(
                offset + count as u64 <= metadata.len(),
                "{} grew while being copied",
                source_path.display()
            );
            temporary
                .write_all(&buffer[..count])
                .context("Writing staged media")?;
            digest.update(&buffer[..count]);
            offset += count as u64;
            control.copied.fetch_add(count as u64, Ordering::Relaxed);
        }
        ensure!(
            offset == metadata.len() && SourceIdentity::from(&source.metadata()?) == identity,
            "{} changed while being copied",
            source_path.display()
        );
        control.check()?;
        temporary
            .as_file()
            .sync_all()
            .context("Syncing staged media")?;
        let digest = format!("{:x}", digest.finalize());
        let object = if let Some(existing) = self.objects.get(&digest) {
            existing.clone()
        } else {
            let object = self.directory.path().join(&digest);
            temporary
                .persist_noclobber(&object)
                .context("Keeping staged media")?;
            self.stored_bytes += offset;
            self.objects.insert(digest.clone(), object.clone());
            object
        };
        let extension = source_path
            .extension()
            .and_then(|value| value.to_str())
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 16
                    && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let filename = format!("{digest}.{extension}");
        let alias = self.directory.path().join(&filename);
        match fs::hard_link(&object, &alias) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::symlink_metadata(&alias)?;
                let expected = fs::metadata(&object)?;
                ensure!(
                    existing.is_file()
                        && existing.dev() == expected.dev()
                        && existing.ino() == expected.ino(),
                    "Staged media alias was replaced"
                );
            }
            Err(error) => return Err(error).context("Creating staged media filename"),
        }
        control.check()?;
        let destination = self.destination.join(filename);
        self.paths.insert(source_path.into(), destination.clone());
        self.sources.insert(source_path.into(), identity);
        Ok(destination)
    }

    pub fn paths(&self) -> &HashMap<PathBuf, PathBuf> {
        &self.paths
    }

    pub fn stored_bytes(&self) -> u64 {
        self.stored_bytes
    }

    pub fn unique_files(&self) -> usize {
        self.objects.len()
    }

    /// These paths are valid only while the staging object is alive.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    pub fn sync(&self) -> Result<()> {
        File::open(self.directory.path())?.sync_all()?;
        self.parent.sync_all()?;
        Ok(())
    }

    pub(crate) fn retain(self) -> Result<String> {
        self.sync()?;
        let name = self
            .directory
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let _ = self.directory.keep();
        Ok(name)
    }

    pub(crate) fn published(self) {
        let _ = self.directory.keep();
    }
}
