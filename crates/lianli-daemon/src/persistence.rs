//! Atomic persistence for configuration, presets, profiles and templates.

use anyhow::{Context, Result};
use lianli_shared::config::AppConfig;
use lianli_shared::rgb::RgbPreset;
use std::fs::{self, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
type WriteKey = (u64, u64, std::ffi::OsString);
static WRITERS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<WriteKey>>> =
    std::sync::LazyLock::new(Default::default);

struct WriteSlot(WriteKey);

impl WriteSlot {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let metadata = fs::metadata(parent)?;
        let key = (
            metadata.dev(),
            metadata.ino(),
            path.file_name()
                .context("State path has no filename")?
                .to_os_string(),
        );
        anyhow::ensure!(
            WRITERS.lock().insert(key.clone()),
            "This settings file is being saved. Retry after it finishes."
        );
        Ok(Self(key))
    }
}

impl Drop for WriteSlot {
    fn drop(&mut self) {
        WRITERS.lock().remove(&self.0);
    }
}

pub fn write_json<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    update_json(path, |_| Ok(value))
}

pub fn update_json<T: serde::Serialize>(
    path: &Path,
    update: impl FnOnce(Option<&[u8]>) -> Result<T>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent directory for {}", path.display()))?;
    let _writer = WriteSlot::acquire(path)?;
    let (previous, permissions) = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => {
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file(),
                "{} is not a regular file",
                path.display()
            );
            let mut previous = Vec::new();
            file.take(MAX_STATE_BYTES as u64 + 1)
                .read_to_end(&mut previous)?;
            anyhow::ensure!(
                previous.len() <= MAX_STATE_BYTES,
                "{} exceeds the 16 MiB backup limit. It was not overwritten.",
                path.display()
            );
            serde_json::from_slice::<serde_json::Value>(&previous).with_context(|| {
                format!(
                    "{} contains invalid JSON. Repair it or restore a backup before saving.",
                    path.display()
                )
            })?;
            let permissions = Permissions::from_mode(metadata.permissions().mode() & 0o777);
            (Some(previous), Some(permissions))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening {} before saving", path.display()))
        }
    };
    let value = update(previous.as_deref())?;
    let json = serde_json::to_string_pretty(&value)?;
    anyhow::ensure!(
        json.len() <= MAX_STATE_BYTES,
        "Configuration exceeds the 16 MiB state-file limit"
    );
    if let Some(previous) = previous {
        replace_file(&backup_path(path), &previous, permissions.clone())?;
    }
    replace_file(path, json.as_bytes(), permissions)
}

pub fn backup_path(path: &Path) -> PathBuf {
    state_backup_path(path, false)
}

pub fn state_backup_path(path: &Path, preserved: bool) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(if preserved { ".before-restore" } else { ".bak" });
    PathBuf::from(name)
}

fn replace_file(path: &Path, bytes: &[u8], permissions: Option<Permissions>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("staging {}", path.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    temporary
        .persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    fs::File::open(parent)?
        .sync_all()
        .with_context(|| format!("syncing the directory for {}", path.display()))?;
    Ok(())
}

/// Serialize and save the daemon's config to disk.
pub fn write_config(path: &Path, config: &AppConfig) -> Result<()> {
    write_json(path, config)
}

/// Load RGB presets from disk (returns an empty vec if the file is missing or
/// unparseable — presets are non-critical state).
pub fn read_rgb_presets(path: &Path) -> Vec<RgbPreset> {
    match fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist RGB presets to disk.
pub fn write_rgb_presets(path: &Path, presets: &[RgbPreset]) -> Result<()> {
    write_json(path, presets)
}

#[cfg(test)]
mod tests {
    #[test]
    fn update_reserves_the_file_before_read_modify_write_and_preserves_failed_updates() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        super::write_json(&path, &serde_json::json!({"value": 1})).unwrap();
        super::update_json(&path, |previous| {
            assert!(super::write_json(&path, &serde_json::json!({"value": 99})).is_err());
            let mut value: serde_json::Value = serde_json::from_slice(previous.unwrap())?;
            value["value"] = serde_json::json!(2);
            Ok(value)
        })
        .unwrap();
        let current = std::fs::read(&path).unwrap();
        let backup = std::fs::read(super::backup_path(&path)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&current).unwrap()["value"],
            2
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&backup).unwrap()["value"],
            1
        );
        assert!(
            super::update_json::<serde_json::Value>(&path, |_| anyhow::bail!("rejected update"))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), current);
        assert_eq!(std::fs::read(super::backup_path(&path)).unwrap(), backup);
    }
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn replacement_keeps_previous_json_and_permissions_without_using_predictable_temporary_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let sentinel = root.path().join("unrelated");
        fs::write(&sentinel, b"untouched").unwrap();
        symlink(&sentinel, path.with_extension("json.tmp")).unwrap();
        write_json(&path, &serde_json::json!({"value": 1})).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        write_json(&path, &serde_json::json!({"value": 2})).unwrap();
        let previous: serde_json::Value =
            serde_json::from_slice(&fs::read(backup_path(&path)).unwrap()).unwrap();
        assert_eq!(previous["value"], 1);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read(sentinel).unwrap(), b"untouched");
    }

    #[test]
    fn write_reservation_rejects_same_file_aliases_without_blocking_other_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        let slot = WriteSlot::acquire(&path).unwrap();
        assert!(write_json(&path, &()).is_err());
        assert!(WriteSlot::acquire(&root.path().join("./state.json")).is_err());
        write_json(&root.path().join("other.json"), &()).unwrap();
        drop(slot);
        write_json(&path, &()).unwrap();
    }

    #[test]
    fn invalid_existing_json_is_not_overwritten_and_does_not_destroy_its_backup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("templates.json");
        write_json(&path, &[1]).unwrap();
        write_json(&path, &[2]).unwrap();
        let previous = fs::read(backup_path(&path)).unwrap();
        fs::write(&path, b"incomplete {").unwrap();
        assert!(write_json(&path, &[3]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"incomplete {");
        assert_eq!(fs::read(backup_path(&path)).unwrap(), previous);
    }
}
