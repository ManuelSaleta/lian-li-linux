use crate::state::StateFile;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const JOURNAL: &str = ".lianli-state-transaction.json";
const LOCK: &str = ".lianli-state-transaction.lock";
const BACKUP_PREFIX: &str = ".lianli-state-backup-";
const MAX_FILE: usize = 16 * 1024 * 1024;
const MAX_STATE: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Content {
    digest: String,
    mode: u32,
    gid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    old: Option<Content>,
    new: Option<Content>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Journal {
    version: u32,
    config_name: String,
    backup: String,
    committed: bool,
    entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media: Option<crate::media_publication::MediaPublication>,
}

struct Directory(File);

struct TransactionLock(File);

impl Drop for TransactionLock {
    fn drop(&mut self) {
        // A forked child may retain this descriptor until exec; close alone delays recovery.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Directory {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "State directory must belong to this account and must not be writable by other accounts");
        Ok(Self(file))
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    fn child(&self, name: &str, create: bool) -> Result<Self> {
        let path = self.path().join(name);
        if create {
            match fs::create_dir(&path) {
                Ok(()) => fs::set_permissions(&path, Permissions::from_mode(0o700))?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Self::open(&path)
    }

    fn verify_path(&self, path: &Path) -> Result<()> {
        let held = self.0.metadata()?;
        let current = fs::metadata(path)?;
        ensure!(
            held.dev() == current.dev() && held.ino() == current.ino(),
            "Destination state directory changed during publication"
        );
        Ok(())
    }

    fn parent_for(&self, path: &Path, create: bool) -> Result<(Self, String)> {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("Invalid state filename")?
            .to_string();
        let directory = if path.parent() == Some(Path::new("profiles")) {
            self.child("profiles", create)?
        } else {
            Self(self.0.try_clone()?)
        };
        Ok((directory, name))
    }

    fn read(&self, path: &Path) -> Result<Option<(Vec<u8>, Content)>> {
        self.read_bounded(path, MAX_FILE)
    }

    fn read_bounded(&self, path: &Path, limit: usize) -> Result<Option<(Vec<u8>, Content)>> {
        let (directory, name) = match self.parent_for(path, false) {
            Ok(value) => value,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(directory.path().join(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
            "State entry must be a regular file owned by this account"
        );
        ensure!(
            metadata.len() <= limit as u64,
            "State file exceeds its read limit"
        );
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= limit,
            "State file grew beyond its read limit"
        );
        let content = Content {
            digest: digest(&bytes),
            mode: metadata.mode() & 0o777,
            gid: metadata.gid(),
        };
        Ok(Some((bytes, content)))
    }

    fn write(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        self.write_with_gid(path, bytes, mode, None)
    }

    fn write_with_gid(&self, path: &Path, bytes: &[u8], mode: u32, gid: Option<u32>) -> Result<()> {
        let (parent, name) = self.parent_for(path, true)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent.path())?;
        temporary.write_all(bytes)?;
        if let Some(gid) = gid {
            ensure!(
                unsafe { libc::fchown(temporary.as_file().as_raw_fd(), u32::MAX, gid) } == 0,
                "Restoring state file group: {}",
                std::io::Error::last_os_error()
            );
        }
        temporary
            .as_file()
            .set_permissions(Permissions::from_mode(mode))?;
        temporary.as_file().sync_all()?;
        temporary.persist(parent.path().join(name))?;
        parent.0.sync_all()?;
        Ok(())
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let (parent, name) = match self.parent_for(path, false) {
            Ok(value) => value,
            Err(error) if missing(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        match fs::remove_file(parent.path().join(name)) {
            Ok(()) => parent.0.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn lock(&self) -> Result<TransactionLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(self.path().join(LOCK))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1,
            "Unsafe state transaction lock"
        );
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Another state transaction is in progress"
        );
        Ok(TransactionLock(file))
    }
}

pub struct StateTransaction {
    root: Directory,
    root_path: PathBuf,
    _lock: TransactionLock,
    journal: Journal,
}

impl StateTransaction {
    /// The caller must stop the destination daemon and reserve hardware ownership
    /// until publication finishes. The destination must support state_recovery.
    /// `files` replaces the complete known state set.
    pub fn prepare(config_path: &Path, files: &[StateFile]) -> Result<Self> {
        Self::prepare_with_attributes(config_path, files, &BTreeMap::new())
    }

    fn prepare_with_attributes(
        config_path: &Path,
        files: &[StateFile],
        attributes: &BTreeMap<PathBuf, Content>,
    ) -> Result<Self> {
        Self::prepare_all(config_path, files, attributes, None)
    }

    pub(crate) fn prepare_media(
        config_path: &Path,
        files: &[StateFile],
        media: crate::media_publication::MediaPublication,
    ) -> Result<Self> {
        Self::prepare_all(config_path, files, &BTreeMap::new(), Some(media))
    }

    fn prepare_all(
        config_path: &Path,
        files: &[StateFile],
        attributes: &BTreeMap<PathBuf, Content>,
        media: Option<crate::media_publication::MediaPublication>,
    ) -> Result<Self> {
        let name = config_name(config_path)?;
        let parent = config_parent(config_path);
        let root_path = if parent.is_absolute() {
            parent.to_path_buf()
        } else {
            std::env::current_dir()?.join(parent)
        };
        let root = Directory::open(&root_path)?;
        let lock = root.lock()?;
        if let Some(media) = &media {
            media.verify_staged(&root.0)?;
        }
        ensure!(
            root.read_bounded(Path::new(JOURNAL), 512 * 1024)?.is_none(),
            "Recover the pending state transaction before preparing another"
        );
        let mut backup_count = 0;
        for (index, entry) in fs::read_dir(root.path())?.enumerate() {
            ensure!(index < 4096, "Too many state directory entries");
            if entry?
                .file_name()
                .to_string_lossy()
                .starts_with(BACKUP_PREFIX)
            {
                backup_count += 1;
            }
        }
        ensure!(
            backup_count < 16,
            "Remove an old migration backup before creating another (16-backup limit)"
        );
        ensure!(files.len() <= 259, "Too many incoming state files");
        ensure!(
            files
                .iter()
                .filter(|file| file.relative_path.parent() == Some(Path::new("profiles")))
                .count()
                <= 256,
            "More than 256 incoming profiles"
        );
        let mut incoming = BTreeMap::new();
        let mut total = 0;
        for file in files {
            validate_path(&file.relative_path, &name)?;
            total += file.bytes.len();
            ensure!(
                file.bytes.len() <= MAX_FILE && total <= MAX_STATE,
                "Incoming state exceeds its size budget"
            );
            let _: serde_json::Value =
                serde_json::from_slice(&file.bytes).context("Incoming state is not valid JSON")?;
            ensure!(
                incoming
                    .insert(file.relative_path.clone(), &file.bytes)
                    .is_none(),
                "Duplicate incoming state file"
            );
        }
        let mut names = known_paths(&root, &name)?;
        ensure!(names.len() <= 259, "More than 256 existing profiles");
        names.extend(incoming.keys().cloned());
        ensure!(names.len() <= 515, "Too many state files");
        let temporary = tempfile::Builder::new()
            .prefix(BACKUP_PREFIX)
            .tempdir_in(root.path())?;
        let backup_name = temporary
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .context("Invalid backup name")?
            .to_string();
        let backup = Directory::open(temporary.path())?;
        let old_dir = backup.child("old", true)?;
        let new_dir = backup.child("new", true)?;
        let mut entries = Vec::new();
        let mut old_total = 0;
        for path in names {
            let old = if let Some((bytes, content)) = root.read(&path)? {
                old_total += bytes.len();
                ensure!(old_total <= MAX_STATE, "Existing state exceeds 64 MiB");
                old_dir.write(&path, &bytes, 0o600)?;
                Some(content)
            } else {
                None
            };
            let new = if let Some(bytes) = incoming.get(&path) {
                new_dir.write(&path, bytes, 0o600)?;
                Some(Content {
                    digest: digest(bytes),
                    mode: attributes.get(&path).map_or(0o600, |content| content.mode),
                    gid: attributes
                        .get(&path)
                        .map_or_else(|| unsafe { libc::getegid() }, |content| content.gid),
                })
            } else {
                None
            };
            entries.push(Entry { path, old, new });
        }
        let journal = Journal {
            version: 1,
            config_name: name,
            backup: backup_name,
            committed: false,
            entries,
            media,
        };
        write_journal(&backup, "manifest.json", &journal)?;
        old_dir.0.sync_all()?;
        new_dir.0.sync_all()?;
        backup.0.sync_all()?;
        // Recovery must retain these files even if journal persistence reports an error.
        let _retained = temporary.keep();
        write_journal(&root, JOURNAL, &journal)?;
        Ok(Self {
            root,
            root_path,
            _lock: lock,
            journal,
        })
    }

    pub fn publish(self) -> Result<String> {
        self.publish_checked(false)
    }

    pub(crate) fn resume_publication(self) -> Result<String> {
        ensure!(
            self.journal.media.is_none(),
            "Only state restoration can resume"
        );
        self.publish_checked(true)
    }

    fn publish_checked(self, allow_partial: bool) -> Result<String> {
        self.root.verify_path(&self.root_path)?;
        ensure!(
            read_journal(&self.root, &self.journal.config_name)? == self.journal,
            "State transaction journal changed"
        );
        verify_current(&self.root, &self.journal, allow_partial)?;
        if let Some(media) = &self.journal.media {
            media.publish(&self.root.0)?;
            verify_current(&self.root, &self.journal, false)?;
        }
        apply(&self.root, &self.journal, false)?;
        for entry in &self.journal.entries {
            ensure!(
                self.root.read(&entry.path)?.map(|(_, content)| content) == entry.new,
                "Published state changed before commit"
            );
        }
        self.root.verify_path(&self.root_path)?;
        if let Some(media) = &self.journal.media {
            media.verify_published(&self.root.0)?;
        }
        let mut committed = self.journal.clone();
        committed.committed = true;
        let backup = self.root.child(&committed.backup, false)?;
        write_journal(&backup, "manifest.json", &committed)?;
        write_journal(&self.root, JOURNAL, &committed)?;
        self.root.remove(Path::new(JOURNAL))?;
        Ok(committed.backup)
    }

    pub(crate) fn backup_name(&self) -> &str {
        &self.journal.backup
    }
}

/// Called with hardware ownership reserved, before loading configuration or
/// starting devices. A live publisher's lock prevents concurrent recovery.
pub fn recover(config_path: &Path) -> Result<Option<String>> {
    let parent = config_parent(config_path);
    match fs::symlink_metadata(parent.join(JOURNAL)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let name = config_name(config_path)?;
    let root = Directory::open(parent)?;
    let _lock = root.lock()?;
    let journal = read_journal(&root, &name)?;
    if !journal.committed {
        verify_current(&root, &journal, true)?;
        apply(&root, &journal, true)?;
    } else if let Some(media) = &journal.media {
        media.verify_published(&root.0)?;
    }
    root.verify_path(parent)?;
    root.remove(Path::new(JOURNAL))?;
    Ok(Some(journal.backup))
}

/// Requires the same stopped-daemon and reserved-ownership conditions as prepare.
/// The replaced state is retained as another backup.
pub fn restore_backup(config_path: &Path, backup: &str) -> Result<String> {
    prepare_restore(config_path, backup)?.publish()
}

pub(crate) fn prepare_restore(config_path: &Path, backup: &str) -> Result<StateTransaction> {
    validate_backup(backup)?;
    let (files, attributes) = {
        let name = config_name(config_path)?;
        let root = Directory::open(config_parent(config_path))?;
        let _lock = root.lock()?;
        ensure!(
            root.read_bounded(Path::new(JOURNAL), 512 * 1024)?.is_none(),
            "Recover the pending transaction before restoring a backup"
        );
        let directory = root.child(backup, false)?;
        let (bytes, _) = directory
            .read_bounded(Path::new("manifest.json"), 512 * 1024)?
            .context("Missing backup manifest")?;
        let journal: Journal = serde_json::from_slice(&bytes)?;
        validate_journal(&journal, &name)?;
        ensure!(
            journal.backup == backup,
            "Backup identity differs from its manifest"
        );
        let files = load_side(&directory, &journal, true)?
            .into_iter()
            .filter_map(|(entry, bytes)| {
                bytes.map(|bytes| StateFile {
                    relative_path: entry.path.clone(),
                    bytes,
                })
            })
            .collect::<Vec<_>>();
        let attributes = journal
            .entries
            .into_iter()
            .filter_map(|entry| entry.old.map(|content| (entry.path, content)))
            .collect();
        (files, attributes)
    };
    StateTransaction::prepare_with_attributes(config_path, &files, &attributes)
}

pub(crate) fn resume_restore(
    config_path: &Path,
    backup: &str,
    undo: &str,
) -> Result<StateTransaction> {
    validate_backup(backup)?;
    validate_backup(undo)?;
    ensure!(
        backup != undo,
        "Restoration cannot overwrite its source backup"
    );
    let name = config_name(config_path)?;
    let root_path = config_parent(config_path).to_path_buf();
    let root = Directory::open(&root_path)?;
    let lock = root.lock()?;
    ensure!(
        root.read_bounded(Path::new(JOURNAL), 512 * 1024)?.is_none(),
        "Recover the pending transaction before resuming restoration"
    );
    let read_manifest = |id: &str| -> Result<Journal> {
        let directory = root.child(id, false)?;
        let (bytes, _) = directory
            .read_bounded(Path::new("manifest.json"), 512 * 1024)?
            .context("Missing restoration backup manifest")?;
        let journal: Journal = serde_json::from_slice(&bytes)?;
        validate_journal(&journal, &name)?;
        ensure!(journal.backup == id, "Restoration backup identity changed");
        Ok(journal)
    };
    let original = read_manifest(backup)?;
    let mut journal = read_manifest(undo)?;
    ensure!(
        journal.media.is_none(),
        "An undo backup cannot import media"
    );
    let side = |record: &Journal, old: bool| -> BTreeMap<PathBuf, Content> {
        record
            .entries
            .iter()
            .filter_map(|entry| {
                let content = if old { &entry.old } else { &entry.new };
                content.clone().map(|content| (entry.path.clone(), content))
            })
            .collect()
    };
    ensure!(
        side(&original, true) == side(&journal, false),
        "Undo backup does not restore the original destination state"
    );
    let undo_directory = root.child(undo, false)?;
    drop(load_side(&undo_directory, &journal, true)?);
    drop(load_side(&undo_directory, &journal, false)?);
    verify_current(&root, &journal, true)?;
    root.verify_path(&root_path)?;
    journal.committed = false;
    write_journal(&root, JOURNAL, &journal)?;
    Ok(StateTransaction {
        root,
        root_path,
        _lock: lock,
        journal,
    })
}

fn known_paths(root: &Directory, config: &str) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::from([
        config.into(),
        "lcd_templates.json".into(),
        "rgb_presets.json".into(),
    ]);
    let directory = match root.child("profiles", false) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(paths),
        Err(error) => return Err(error),
    };
    for (index, entry) in fs::read_dir(directory.path())?.enumerate() {
        ensure!(index < 4096, "Too many profile directory entries");
        let path = Path::new("profiles").join(entry?.file_name());
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            validate_path(&path, config)?;
            paths.insert(path);
            ensure!(paths.len() <= 515, "Too many saved profile entries");
        }
    }
    Ok(paths)
}

fn config_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("Invalid configuration filename")?;
    ensure!(
        !name.starts_with(".lianli-")
            && !["lcd_templates.json", "rgb_presets.json"].contains(&name),
        "Configuration filename conflicts with reserved state"
    );
    Ok(name.into())
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub(crate) fn validate_path(path: &Path, config: &str) -> Result<()> {
    if [
        Path::new(config),
        Path::new("lcd_templates.json"),
        Path::new("rgb_presets.json"),
    ]
    .contains(&path)
    {
        return Ok(());
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("Invalid profile filename")?;
    let stem = name
        .strip_suffix(".json")
        .context("Unsupported state file")?;
    ensure!(
        path == Path::new("profiles").join(name)
            && !stem.is_empty()
            && stem.len() <= 250
            && ![".", ".."].contains(&stem)
            && !stem.contains(['/', '\\', '\0']),
        "Unsafe profile path"
    );
    Ok(())
}

pub(crate) fn validate_backup(name: &str) -> Result<()> {
    let suffix = name
        .strip_prefix(BACKUP_PREFIX)
        .context("Invalid backup identifier")?;
    ensure!(
        !suffix.is_empty()
            && suffix.len() <= 32
            && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "Invalid backup identifier"
    );
    Ok(())
}

fn validate_journal(journal: &Journal, config: &str) -> Result<()> {
    ensure!(
        journal.version == 1 && journal.config_name == config && journal.entries.len() <= 515,
        "Unsupported state transaction manifest"
    );
    validate_backup(&journal.backup)?;
    if let Some(media) = &journal.media {
        media.validate()?;
    }
    for old in [true, false] {
        ensure!(
            journal
                .entries
                .iter()
                .filter(|entry| entry.path.parent() == Some(Path::new("profiles"))
                    && if old {
                        entry.old.is_some()
                    } else {
                        entry.new.is_some()
                    })
                .count()
                <= 256,
            "Transaction exceeds the per-state profile limit"
        );
    }
    let mut paths = BTreeSet::new();
    for entry in &journal.entries {
        validate_path(&entry.path, config)?;
        ensure!(paths.insert(&entry.path), "Duplicate transaction path");
        for content in [&entry.old, &entry.new].into_iter().flatten() {
            ensure!(
                content.gid != u32::MAX
                    && content.mode <= 0o777
                    && content.digest.len() == 64
                    && content.digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid state transaction digest or permissions"
            );
        }
    }
    Ok(())
}

fn write_journal(root: &Directory, name: &str, journal: &Journal) -> Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    ensure!(
        bytes.len() <= 512 * 1024,
        "State transaction manifest exceeds 512 KiB"
    );
    root.write(Path::new(name), &bytes, 0o600)
}

fn read_journal(root: &Directory, config: &str) -> Result<Journal> {
    let (bytes, _) = root
        .read_bounded(Path::new(JOURNAL), 512 * 1024)?
        .context("Missing state transaction journal")?;
    ensure!(
        bytes.len() <= 512 * 1024,
        "State transaction journal exceeds 512 KiB"
    );
    let journal = serde_json::from_slice(&bytes)?;
    validate_journal(&journal, config)?;
    Ok(journal)
}

fn verify_current(root: &Directory, journal: &Journal, allow_new: bool) -> Result<()> {
    let expected: BTreeSet<_> = journal
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    ensure!(
        known_paths(root, &journal.config_name)?.is_subset(&expected),
        "New state files appeared outside the transaction. Preserve them and recover manually"
    );
    for entry in &journal.entries {
        let current = root.read(&entry.path)?.map(|(_, content)| content);
        ensure!(
            current == entry.old || (allow_new && current == entry.new),
            "{} changed outside the transaction. Preserve it and recover manually",
            entry.path.display()
        );
    }
    Ok(())
}

type SavedSide<'a> = Vec<(&'a Entry, Option<Vec<u8>>)>;

fn load_side<'a>(backup: &Directory, journal: &'a Journal, old: bool) -> Result<SavedSide<'a>> {
    let directory = backup.child(if old { "old" } else { "new" }, false)?;
    let mut saved = Vec::new();
    let mut total = 0;
    for entry in &journal.entries {
        let content = if old { &entry.old } else { &entry.new };
        let bytes = match content {
            Some(content) => {
                let (bytes, actual) = directory
                    .read(&entry.path)?
                    .context("Missing staged state file")?;
                ensure!(
                    actual.digest == content.digest,
                    "{} backup content changed",
                    entry.path.display()
                );
                total += bytes.len();
                ensure!(total <= MAX_STATE, "Saved state exceeds 64 MiB");
                Some(bytes)
            }
            None => None,
        };
        saved.push((entry, bytes));
    }
    Ok(saved)
}

fn apply(root: &Directory, journal: &Journal, old: bool) -> Result<()> {
    let backup = root.child(&journal.backup, false)?;
    let saved = load_side(&backup, journal, old)?;
    for (entry, bytes) in saved {
        match bytes {
            Some(bytes) => {
                let content = if old {
                    entry.old.as_ref()
                } else {
                    entry.new.as_ref()
                }
                .unwrap();
                root.write_with_gid(&entry.path, &bytes, content.mode, Some(content.gid))?;
            }
            None => root.remove(&entry.path)?,
        }
    }
    root.0.sync_all()?;
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    fn state(path: &str, value: serde_json::Value) -> StateFile {
        StateFile {
            relative_path: path.into(),
            bytes: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn media(root: &Path) -> crate::media_publication::MediaPublication {
        let id = "0123456789abcdef0123456789abcdef";
        let preparation = format!(".lianli-migration-{id}-abcdef");
        let media = ".lianli-media-abcdef";
        let directory = root.join(&preparation).join(media);
        fs::create_dir_all(&directory).unwrap();
        let bytes = b"fixture pixels";
        let file = directory.join(digest(bytes));
        fs::write(&file, bytes).unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
        crate::media_publication::MediaPublication {
            id: id.into(),
            preparation,
            media: media.into(),
            fingerprint: crate::media_publication::fingerprint(&directory).unwrap(),
        }
    }

    #[test]
    fn media_publication_recovers_before_and_after_settings_write_without_removing_imports() {
        for write_state in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config.json");
            fs::write(&config, b"{}").unwrap();
            let media = media(root.path());
            let files = [state(
                "config.json",
                json!({"path":root.path().join("media/imports").join(&media.id)}),
            )];
            let transaction =
                StateTransaction::prepare_media(&config, &files, media.clone()).unwrap();
            media.publish(&transaction.root.0).unwrap();
            media.publish(&transaction.root.0).unwrap();
            if write_state {
                apply(&transaction.root, &transaction.journal, false).unwrap();
            }
            drop(transaction);
            recover(&config).unwrap();
            assert_eq!(fs::read(&config).unwrap(), b"{}");
            media
                .verify_published(&Directory::open(root.path()).unwrap().0)
                .unwrap();
        }
    }

    #[test]
    fn committed_media_recovery_refuses_missing_or_changed_imports() {
        for remove in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config.json");
            fs::write(&config, b"{}").unwrap();
            let media = media(root.path());
            let transaction = StateTransaction::prepare_media(
                &config,
                &[state("config.json", json!({"new":true}))],
                media.clone(),
            )
            .unwrap();
            media.publish(&transaction.root.0).unwrap();
            apply(&transaction.root, &transaction.journal, false).unwrap();
            let mut journal = transaction.journal.clone();
            journal.committed = true;
            write_journal(&transaction.root, JOURNAL, &journal).unwrap();
            drop(transaction);
            let path = root.path().join("media/imports").join(media.id);
            if remove {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::write(
                    fs::read_dir(path).unwrap().next().unwrap().unwrap().path(),
                    b"changed",
                )
                .unwrap();
            }
            assert!(recover(&config).is_err());
            assert!(root.path().join(JOURNAL).exists());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&fs::read(config).unwrap()).unwrap()
                    ["new"],
                true
            );
        }
    }

    #[test]
    fn existing_imports_are_never_replaced_and_failed_publication_preserves_state() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let media = media(root.path());
        let transaction = StateTransaction::prepare_media(
            &config,
            &[state("config.json", json!({"new":true}))],
            media.clone(),
        )
        .unwrap();
        let existing = root.path().join("media/imports").join(&media.id);
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join("keep"), b"other files").unwrap();
        assert!(transaction.publish().is_err());
        recover(&config).unwrap();
        assert_eq!(fs::read(&config).unwrap(), b"{}");
        assert_eq!(fs::read(existing.join("keep")).unwrap(), b"other files");
        assert!(root
            .path()
            .join(media.preparation)
            .join(media.media)
            .exists());
    }

    #[test]
    fn resumed_restoration_reuses_its_backup_across_every_publication_boundary() {
        for interrupted_at in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config.json");
            fs::write(&config, b"{ \"original\": true }\n").unwrap();
            fs::set_permissions(&config, Permissions::from_mode(0o640)).unwrap();
            fs::create_dir(root.path().join("profiles")).unwrap();
            fs::write(root.path().join("profiles/old.json"), b"{}").unwrap();
            let original = StateTransaction::prepare(
                &config,
                &[
                    state("config.json", json!({"migrated": true})),
                    state("profiles/new.json", json!({"new": true})),
                ],
            )
            .unwrap()
            .publish()
            .unwrap();
            let transaction = prepare_restore(&config, &original).unwrap();
            let undo = transaction.backup_name().to_string();
            match interrupted_at {
                0 => drop(transaction),
                1 => {
                    let entry = transaction
                        .journal
                        .entries
                        .iter()
                        .find(|entry| entry.path == Path::new("config.json"))
                        .unwrap();
                    let content = entry.new.as_ref().unwrap();
                    transaction
                        .root
                        .write_with_gid(
                            &entry.path,
                            b"{ \"original\": true }\n",
                            content.mode,
                            Some(content.gid),
                        )
                        .unwrap();
                    drop(transaction);
                }
                2 => {
                    apply(&transaction.root, &transaction.journal, false).unwrap();
                    let mut committed = transaction.journal.clone();
                    committed.committed = true;
                    write_journal(&transaction.root, JOURNAL, &committed).unwrap();
                    drop(transaction);
                }
                _ => {
                    transaction.publish().unwrap();
                }
            }
            recover(&config).unwrap();
            for _ in 0..2 {
                assert_eq!(
                    resume_restore(&config, &original, &undo)
                        .unwrap()
                        .resume_publication()
                        .unwrap(),
                    undo
                );
                assert_eq!(fs::read(&config).unwrap(), b"{ \"original\": true }\n");
                assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o640);
                assert!(root.path().join("profiles/old.json").exists());
                assert!(!root.path().join("profiles/new.json").exists());
                assert!(!root.path().join(JOURNAL).exists());
                assert_eq!(
                    fs::read_dir(root.path())
                        .unwrap()
                        .filter(|entry| entry
                            .as_ref()
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with(BACKUP_PREFIX))
                        .count(),
                    2
                );
            }
            fs::write(&config, b"{\"external\":true}").unwrap();
            assert!(resume_restore(&config, &original, &undo).is_err());
            assert_eq!(fs::read(&config).unwrap(), b"{\"external\":true}");
            assert!(!root.path().join(JOURNAL).exists());
        }
    }

    #[test]
    fn resumed_restoration_rejects_an_unrelated_undo_and_damaged_saved_state() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let original =
            StateTransaction::prepare(&config, &[state("config.json", json!({"migrated": true}))])
                .unwrap()
                .publish()
                .unwrap();
        let unrelated =
            StateTransaction::prepare(&config, &[state("config.json", json!({"unrelated": true}))])
                .unwrap()
                .publish()
                .unwrap();
        assert!(resume_restore(&config, &original, &unrelated).is_err());
        assert!(!root.path().join(JOURNAL).exists());
        let undo = prepare_restore(&config, &original)
            .unwrap()
            .publish()
            .unwrap();
        fs::write(
            root.path().join(&undo).join("new/config.json"),
            b"{\"damaged\":true}",
        )
        .unwrap();
        assert!(resume_restore(&config, &original, &undo).is_err());
        assert_eq!(fs::read(&config).unwrap(), b"{}");
        assert!(!root.path().join(JOURNAL).exists());
    }

    #[test]
    fn publication_and_explicit_restore_preserve_original_state_permissions_and_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let original = b"{ \"future\": true }\n";
        fs::write(&config, original).unwrap();
        fs::set_permissions(&config, Permissions::from_mode(0o640)).unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        fs::write(root.path().join("profiles/old.json"), b"{}").unwrap();
        fs::write(root.path().join("notes.txt"), b"keep").unwrap();
        let files = [
            state("config.json", json!({"hardware_video":true})),
            state(
                "profiles/new.json",
                json!({"name":"New","device_id":"offline"}),
            ),
        ];
        let backup = StateTransaction::prepare(&config, &files)
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(fs::read(&config).unwrap(), files[0].bytes);
        assert!(!root.path().join("profiles/old.json").exists());
        assert!(root.path().join("profiles/new.json").exists());
        assert!(!root.path().join(JOURNAL).exists());
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o600);
        restore_backup(&config, &backup).unwrap();
        assert_eq!(fs::read(&config).unwrap(), original);
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o640);
        assert!(root.path().join("profiles/old.json").exists());
        assert!(!root.path().join("profiles/new.json").exists());
        assert_eq!(fs::read(root.path().join("notes.txt")).unwrap(), b"keep");
    }

    #[test]
    fn interrupted_partial_publication_rolls_back_before_restart_and_live_work_is_excluded() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let files = [
            state("config.json", json!({"hardware_video":true})),
            state("lcd_templates.json", json!({"templates":[]})),
        ];
        let transaction = StateTransaction::prepare(&config, &files).unwrap();
        assert!(recover(&config)
            .unwrap_err()
            .to_string()
            .contains("in progress"));
        assert!(StateTransaction::prepare(&config, &files).is_err());
        transaction
            .root
            .write_with_gid(
                Path::new("config.json"),
                &files[0].bytes,
                0o600,
                Some(unsafe { libc::getegid() }),
            )
            .unwrap();
        let backup = transaction.journal.backup.clone();
        drop(transaction);
        assert_eq!(recover(&config).unwrap(), Some(backup));
        assert_eq!(fs::read(&config).unwrap(), b"{}");
        assert!(!root.path().join("lcd_templates.json").exists());
        assert_eq!(recover(&config).unwrap(), None);
    }

    #[test]
    fn committed_marker_finishes_cleanup_without_rolling_back_new_state() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let files = [state("config.json", json!({"hardware_video":true}))];
        let transaction = StateTransaction::prepare(&config, &files).unwrap();
        apply(&transaction.root, &transaction.journal, false).unwrap();
        let mut journal = transaction.journal.clone();
        journal.committed = true;
        write_journal(&transaction.root, JOURNAL, &journal).unwrap();
        drop(transaction);
        recover(&config).unwrap();
        assert_eq!(fs::read(&config).unwrap(), files[0].bytes);
    }

    #[test]
    fn refuses_external_edits_and_corrupt_backups_without_overwriting_state() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let files = [state("config.json", json!({"hardware_video":true}))];
        let transaction = StateTransaction::prepare(&config, &files).unwrap();
        let backup = transaction.journal.backup.clone();
        drop(transaction);
        fs::write(&config, b"{\"external\":true}").unwrap();
        assert!(recover(&config)
            .unwrap_err()
            .to_string()
            .contains("outside the transaction"));
        assert_eq!(fs::read(&config).unwrap(), b"{\"external\":true}");
        fs::write(&config, b"{}").unwrap();
        fs::write(
            root.path().join(&backup).join("old/config.json"),
            b"corrupt",
        )
        .unwrap();
        assert!(recover(&config)
            .unwrap_err()
            .to_string()
            .contains("backup content changed"));
        assert_eq!(fs::read(&config).unwrap(), b"{}");
        assert!(root.path().join(JOURNAL).exists());
    }

    #[test]
    fn refuses_traversal_and_symlinked_profile_directories() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        assert!(
            StateTransaction::prepare(&config, &[state("../outside.json", json!({}))]).is_err()
        );
        symlink(outside.path(), root.path().join("profiles")).unwrap();
        assert!(StateTransaction::prepare(&config, &[state("config.json", json!({}))]).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(!root.path().join(JOURNAL).exists());
    }

    #[test]
    fn startup_without_a_journal_does_not_create_files_or_require_a_writable_config_directory() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        fs::set_permissions(root.path(), Permissions::from_mode(0o500)).unwrap();
        assert_eq!(recover(&config).unwrap(), None);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        fs::set_permissions(root.path(), Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            recover(&root.path().join("missing/config.json")).unwrap(),
            None
        );
    }

    #[test]
    fn replacement_of_the_destination_directory_aborts_publication_without_redirecting_writes() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let config = destination.join("config.json");
        fs::write(&config, b"{}").unwrap();
        let transaction = StateTransaction::prepare(
            &config,
            &[state("config.json", json!({"hardware_video":true}))],
        )
        .unwrap();
        let moved = root.path().join("moved");
        let inherited = transaction._lock.0.try_clone().unwrap();
        fs::rename(&destination, &moved).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(&config, b"{\"external\":true}").unwrap();
        assert!(transaction
            .publish()
            .unwrap_err()
            .to_string()
            .contains("directory changed"));
        assert_eq!(fs::read(&config).unwrap(), b"{\"external\":true}");
        assert_eq!(fs::read(moved.join("config.json")).unwrap(), b"{}");
        recover(&moved.join("config.json")).unwrap();
        drop(inherited);
    }

    #[test]
    fn recovery_allows_the_union_of_old_and_new_profile_names_during_partial_publication() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        for index in 0..128 {
            fs::write(
                root.path().join(format!("profiles/z-old-{index}.json")),
                b"{\"name\":\"Old\",\"device_id\":\"offline\"}",
            )
            .unwrap();
        }
        let mut files = vec![state("config.json", json!({}))];
        files.extend((0..129).map(|index| {
            state(
                &format!("profiles/a-new-{index}.json"),
                json!({"name":"New","device_id":"offline"}),
            )
        }));
        let transaction = StateTransaction::prepare(&config, &files).unwrap();
        for file in files.iter().skip(1) {
            transaction
                .root
                .write_with_gid(
                    &file.relative_path,
                    &file.bytes,
                    0o600,
                    Some(unsafe { libc::getegid() }),
                )
                .unwrap();
        }
        assert_eq!(
            fs::read_dir(root.path().join("profiles")).unwrap().count(),
            257
        );
        drop(transaction);
        recover(&config).unwrap();
        assert_eq!(
            fs::read_dir(root.path().join("profiles")).unwrap().count(),
            128
        );
        assert!(root.path().join("profiles/z-old-0.json").exists());
        assert!(!root.path().join("profiles/a-new-0.json").exists());
    }
}
