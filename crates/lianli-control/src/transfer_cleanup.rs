use crate::account::Account;
use crate::media_publication::Directory;
use crate::reservation::ServiceOperationLock;
use crate::state_transfer::PreparedTransfer;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

const LIMIT: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    prepared: PreparedTransfer,
    parent_inode: u64,
    stage_inode: u64,
}

struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn finish_as(
    account: &Account,
    prepared: &PreparedTransfer,
    operation: &ServiceOperationLock,
) -> Result<()> {
    if let Some(execution) = &account.container {
        execution.destination.check_config(&prepared.config_path)?;
    }
    ensure!(
        account.uid == prepared.destination_uid,
        "Cleanup account differs from the prepared transfer"
    );
    operation.verify()?;
    let bytes = serde_json::to_vec(prepared)?;
    ensure!(bytes.len() <= LIMIT, "Cleanup request exceeds 64 KiB");
    let mut input = crate::state_transfer::sealed_state(&bytes)?;
    input.rewind()?;
    let output = crate::command::run_with_stdin(
        account.control_command(&[OsStr::new("finish-transfer")])?,
        Stdio::from(input),
        Duration::from_secs(120),
    )?;
    operation.verify()?;
    ensure!(
        output.status.success() && output.stdout.is_empty(),
        "Service mode is finalized but preparation cleanup remains pending: {}",
        output.stderr.trim()
    );
    Ok(())
}

pub fn serve() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Remove prepared copies under their unprivileged account"
    );
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT, "Cleanup request exceeds 64 KiB");
    let prepared: PreparedTransfer = serde_json::from_slice(&bytes)?;
    crate::container_destination::verify_config(&prepared.config_path)?;
    finish(&prepared)
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1
            && metadata.len() <= LIMIT as u64,
        "Invalid cleanup or preparation manifest. Preserve it for inspection"
    );
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT, "Cleanup manifest grew beyond 64 KiB");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn verify_parent(root: &Directory, path: &Path) -> Result<()> {
    let held = root.0.metadata()?;
    let current = fs::symlink_metadata(path)?;
    ensure!(
        current.is_dir() && (current.dev(), current.ino()) == (held.dev(), held.ino()),
        "Cleanup configuration directory was replaced"
    );
    Ok(())
}

fn prepare(
    root: &Directory,
    expected: &PreparedTransfer,
    marker: &str,
    trash: &str,
) -> Result<Option<Receipt>> {
    if let Some(value) = read_json(&root.path().join(marker))? {
        let receipt: Receipt = serde_json::from_value(value)?;
        ensure!(
            receipt.version == 1
                && &receipt.prepared == expected
                && receipt.parent_inode == root.0.metadata()?.ino(),
            "Cleanup receipt belongs to another preparation or directory"
        );
        return Ok(Some(receipt));
    }
    ensure!(
        !exists(&root.path().join(trash))?,
        "Cleanup directory exists without its receipt. Preserve it for inspection"
    );
    if !exists(&root.path().join(&expected.directory))? {
        return Ok(None);
    }
    let stage = root.child(&expected.directory, false)?;
    ensure!(
        stage.0.metadata()?.mode() & 0o077 == 0,
        "Preparation directory is not private"
    );
    let mut manifest = read_json(&stage.path().join("manifest.json"))?
        .context("Preparation manifest is missing")?;
    let object = manifest
        .as_object_mut()
        .context("Invalid preparation manifest")?;
    if let Some(started) = object.remove("publication_started") {
        ensure!(started == true, "Invalid publication marker");
        let backup = object
            .remove("backup")
            .context("Publication backup identity is missing")?;
        crate::state_transaction::validate_backup(
            backup.as_str().context("Invalid publication backup")?,
        )?;
    }
    ensure!(
        serde_json::from_value::<PreparedTransfer>(manifest)? == *expected,
        "Preparation receipt changed. Preserve it before cleanup"
    );
    let validated = inventory(stage, Role::Root, expected, &mut 0)?;
    let receipt = Receipt {
        version: 1,
        prepared: expected.clone(),
        parent_inode: root.0.metadata()?.ino(),
        stage_inode: validated.directory.0.metadata()?.ino(),
    };
    let bytes = serde_json::to_vec(&receipt)?;
    ensure!(bytes.len() <= LIMIT, "Cleanup receipt exceeds 64 KiB");
    let mut file = tempfile::NamedTempFile::new_in(root.path())?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(root.path().join(marker))?;
    root.0.sync_all()?;
    Ok(Some(receipt))
}

fn finish(expected: &PreparedTransfer) -> Result<()> {
    crate::state_transfer::validate_preparation(expected, &expected.config_path, &expected.id)?;
    ensure!(
        expected.destination_uid == unsafe { libc::geteuid() },
        "Preparation belongs to another account"
    );
    let parent = expected
        .config_path
        .parent()
        .context("Preparation has no state directory")?;
    ensure!(
        parent.is_absolute(),
        "Cleanup requires an absolute configuration directory"
    );
    let root = Directory::open(parent)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(root.path().join(".lianli-cleanup.lock"))?;
    let metadata = lock.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == expected.destination_uid
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "Invalid preparation cleanup lock"
    );
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another preparation cleanup is in progress"
    );
    let _lock = Lock(lock);
    ensure!(
        !exists(&root.path().join(".lianli-state-transaction.json"))?,
        "Recover pending state publication before cleaning prepared copies"
    );
    let marker = format!(".lianli-cleanup-{}.json", expected.id);
    let trash = format!(".lianli-migration-{}-finished", expected.id);
    ensure!(
        expected.directory != trash,
        "Preparation and cleanup directory names must differ"
    );
    let Some(receipt) = prepare(&root, expected, &marker, &trash)? else {
        return Ok(());
    };
    verify_parent(&root, parent)?;
    if exists(&root.path().join(&expected.directory))? {
        ensure!(
            !exists(&root.path().join(&trash))?,
            "Both preparation and cleanup directories exist. Preserve them for inspection"
        );
        let source = root.child(&expected.directory, false)?;
        ensure!(
            source.0.metadata()?.ino() == receipt.stage_inode,
            "Preparation directory changed before cleanup"
        );
        let name = CString::new(expected.directory.as_bytes())?;
        let target = CString::new(trash.as_bytes())?;
        ensure!(
            unsafe {
                libc::renameat2(
                    root.0.as_raw_fd(),
                    name.as_ptr(),
                    root.0.as_raw_fd(),
                    target.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } == 0,
            "Cannot isolate completed preparation: {}",
            std::io::Error::last_os_error()
        );
        root.0.sync_all()?;
    }
    if exists(&root.path().join(&trash))? {
        let directory = root.child(&trash, false)?;
        ensure!(
            directory.0.metadata()?.ino() == receipt.stage_inode,
            "Cleanup directory was replaced"
        );
        let tree = inventory(directory, Role::Root, expected, &mut 0)?;
        purge(&tree)?;
        verify_parent(&root, parent)?;
        verify_entry(&root, &trash, &tree.directory.0.metadata()?)?;
        fs::remove_dir(root.path().join(&trash))?;
        root.0.sync_all()?;
    }
    verify_parent(&root, parent)?;
    fs::remove_file(root.path().join(marker))?;
    root.0.sync_all()?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Role {
    Root,
    State,
    Profiles,
    Media,
}

struct Tree {
    directory: Directory,
    entries: Vec<Entry>,
}
struct Entry {
    name: String,
    metadata: fs::Metadata,
    children: Option<Tree>,
}

fn inventory(
    directory: Directory,
    role: Role,
    expected: &PreparedTransfer,
    count: &mut usize,
) -> Result<Tree> {
    ensure!(
        directory.0.metadata()?.mode() & 0o077 == 0,
        "Cleanup directory is not private"
    );
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory.path())? {
        *count += 1;
        ensure!(
            *count <= 8500,
            "Preparation cleanup exceeds its entry limit"
        );
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid preparation filename"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.uid() == expected.destination_uid && metadata.mode() & 0o077 == 0,
            "Preparation entry has unexpected ownership or permissions"
        );
        let child_role = match (role, name.as_str()) {
            (Role::Root, "state") => Some(Role::State),
            (Role::Root, name) if name == expected.media_directory => Some(Role::Media),
            (Role::State, "profiles") => Some(Role::Profiles),
            _ => None,
        };
        let children = if let Some(child_role) = child_role {
            ensure!(
                metadata.is_dir(),
                "Preparation directory was replaced by another file type"
            );
            let child = directory.child(&name, false)?;
            let opened = child.0.metadata()?;
            ensure!(
                (opened.dev(), opened.ino()) == (metadata.dev(), metadata.ino()),
                "Preparation entry changed while opening"
            );
            Some(inventory(child, child_role, expected, count)?)
        } else {
            ensure!(
                metadata.is_file(),
                "Preparation contains a link or special file. Preserve it for inspection"
            );
            match role {
                Role::Root => ensure!(
                    name == "manifest.json"
                        || name
                            .strip_prefix(".tmp")
                            .is_some_and(|suffix| (6..=32).contains(&suffix.len())
                                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())),
                    "Unexpected preparation metadata file"
                ),
                Role::State | Role::Profiles => {
                    let path = if matches!(role, Role::Profiles) {
                        Path::new("profiles").join(&name)
                    } else {
                        name.clone().into()
                    };
                    crate::state_transaction::validate_path(
                        &path,
                        expected
                            .config_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .context("Invalid configuration name")?,
                    )?;
                }
                Role::Media => {
                    let (hash, extension) = name
                        .split_once('.')
                        .map_or((name.as_str(), None), |(hash, ext)| (hash, Some(ext)));
                    ensure!(
                        hash.len() == 64
                            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && extension.is_none_or(|ext| (1..=16).contains(&ext.len())
                                && ext.bytes().all(|byte| byte.is_ascii_alphanumeric())),
                        "Unexpected staged media filename"
                    );
                }
            }
            None
        };
        entries.push(Entry {
            name,
            metadata,
            children,
        });
    }
    Ok(Tree { directory, entries })
}

fn verify_entry(parent: &Directory, name: &str, expected: &fs::Metadata) -> Result<()> {
    let current = fs::symlink_metadata(parent.path().join(name))?;
    ensure!(
        (current.dev(), current.ino(), current.mode())
            == (expected.dev(), expected.ino(), expected.mode()),
        "Preparation entry changed before removal"
    );
    Ok(())
}

fn purge(tree: &Tree) -> Result<()> {
    for entry in &tree.entries {
        verify_entry(&tree.directory, &entry.name, &entry.metadata)?;
        if let Some(child) = &entry.children {
            purge(child)?;
            verify_entry(&tree.directory, &entry.name, &entry.metadata)?;
            fs::remove_dir(tree.directory.path().join(&entry.name))?;
        } else {
            fs::remove_file(tree.directory.path().join(&entry.name))?;
        }
    }
    tree.directory.0.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn names(expected: &PreparedTransfer) -> (String, String) {
        (
            format!(".lianli-cleanup-{}.json", expected.id),
            format!(".lianli-migration-{}-finished", expected.id),
        )
    }

    #[test]
    fn published_preparation_cleanup_preserves_current_state_imports_backups_and_sources() {
        let (source, target, prepared) = crate::state_transfer::prepared_fixture();
        let original = fs::read(source.path().join("config.json")).unwrap();
        let backup = crate::state_transfer::begin_publication(&prepared)
            .unwrap()
            .publish()
            .unwrap();
        let current = fs::read(&prepared.config_path).unwrap();
        let saved = fs::read(target.path().join(&backup).join("manifest.json")).unwrap();
        let imports: Vec<_> = fs::read_dir(&prepared.final_media_path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        finish(&prepared).unwrap();
        finish(&prepared).unwrap();
        assert!(!target.path().join(&prepared.directory).exists());
        let (marker, trash) = names(&prepared);
        for name in [marker, trash] {
            assert!(!target.path().join(name).exists());
        }
        assert_eq!(fs::read(&prepared.config_path).unwrap(), current);
        assert_eq!(
            fs::read(target.path().join(&backup).join("manifest.json")).unwrap(),
            saved
        );
        assert_eq!(
            fs::read(source.path().join("config.json")).unwrap(),
            original
        );
        for (path, bytes) in imports {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert!(!fs::read_dir(target.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".lianli-migration-")));
        crate::state_transaction::restore_backup(&prepared.config_path, &backup).unwrap();
        assert_eq!(
            fs::read(&prepared.config_path).unwrap(),
            b"{\"previous\":true}"
        );
    }

    #[test]
    fn cleanup_restarts_before_rename_after_partial_removal_and_after_directory_removal() {
        for interrupted_at in 0..4 {
            let (_source, target, expected) = crate::state_transfer::prepared_fixture();
            let original = fs::read(&expected.config_path).unwrap();
            let root = Directory::open(target.path()).unwrap();
            let (marker, trash) = names(&expected);
            prepare(&root, &expected, &marker, &trash).unwrap().unwrap();
            if interrupted_at > 0 {
                fs::rename(
                    root.path().join(&expected.directory),
                    root.path().join(&trash),
                )
                .unwrap();
            }
            if interrupted_at == 2 {
                fs::remove_file(root.path().join(&trash).join("manifest.json")).unwrap();
                fs::remove_file(root.path().join(&trash).join("state/config.json")).unwrap();
            }
            if interrupted_at == 3 {
                fs::remove_dir_all(root.path().join(&trash)).unwrap();
            }
            finish(&expected).unwrap();
            assert!(!target.path().join(marker).exists());
            assert!(!target.path().join(trash).exists());
            assert!(!target.path().join(&expected.directory).exists());
            assert_eq!(fs::read(&expected.config_path).unwrap(), original);
        }
    }

    #[test]
    fn unknown_files_links_and_replaced_directories_are_preserved() {
        let (_source, target, expected) = crate::state_transfer::prepared_fixture();
        let stage = target.path().join(&expected.directory);
        let unknown = stage.join("personal.txt");
        fs::write(&unknown, b"preserve").unwrap();
        fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(finish(&expected).is_err());
        assert_eq!(fs::read(&unknown).unwrap(), b"preserve");
        fs::remove_file(&unknown).unwrap();
        symlink(&expected.config_path, &unknown).unwrap();
        assert!(finish(&expected).is_err());
        assert!(fs::symlink_metadata(&unknown).unwrap().is_symlink());
        fs::remove_file(&unknown).unwrap();
        let root = Directory::open(target.path()).unwrap();
        let (marker, trash) = names(&expected);
        prepare(&root, &expected, &marker, &trash).unwrap();
        fs::rename(&stage, target.path().join("retained")).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(stage.join("personal.txt"), b"replacement").unwrap();
        assert!(finish(&expected).is_err());
        assert_eq!(
            fs::read(stage.join("personal.txt")).unwrap(),
            b"replacement"
        );
        assert!(target.path().join("retained/manifest.json").exists());
        assert!(target.path().join(marker).exists());
    }

    #[test]
    fn pending_publication_blocks_cleanup_until_state_recovery_finishes() {
        let (_source, target, expected) = crate::state_transfer::prepared_fixture();
        let transaction = crate::state_transfer::begin_publication(&expected).unwrap();
        assert!(finish(&expected).is_err());
        assert!(target
            .path()
            .join(&expected.directory)
            .join("state/config.json")
            .exists());
        drop(transaction);
        crate::state_transaction::recover(&expected.config_path).unwrap();
        finish(&expected).unwrap();
        assert!(!target.path().join(&expected.directory).exists());
        assert_eq!(
            fs::read(&expected.config_path).unwrap(),
            b"{\"previous\":true}"
        );
    }

    #[test]
    fn changing_the_receipt_or_a_nested_directory_aborts_cleanup() {
        let (_source, target, expected) = crate::state_transfer::prepared_fixture();
        let root = Directory::open(target.path()).unwrap();
        let (marker, trash) = names(&expected);
        prepare(&root, &expected, &marker, &trash).unwrap();
        let mut changed = expected.clone();
        changed.media_bytes += 1;
        assert!(finish(&changed).is_err());
        let stage = root.child(&expected.directory, false).unwrap();
        let tree = inventory(stage, Role::Root, &expected, &mut 0).unwrap();
        let state = root.path().join(&expected.directory).join("state");
        fs::rename(&state, target.path().join("retained-state")).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(state.join("personal.txt"), b"keep").unwrap();
        assert!(purge(&tree).is_err());
        assert_eq!(fs::read(state.join("personal.txt")).unwrap(), b"keep");
        assert!(target.path().join("retained-state/config.json").exists());
    }
}
