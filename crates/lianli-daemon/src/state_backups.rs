use anyhow::{ensure, Context, Result};
use lianli_shared::backups::{BackupEntry, BackupPreview, BackupTarget};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct Locations {
    pub config: PathBuf,
    pub templates: PathBuf,
    pub presets: PathBuf,
}

impl Locations {
    fn original(&self, target: &BackupTarget) -> Result<PathBuf> {
        Ok(match target {
            BackupTarget::Configuration => self.config.clone(),
            BackupTarget::Templates => self.templates.clone(),
            BackupTarget::RgbPresets => self.presets.clone(),
            BackupTarget::Profile { name } => {
                ensure!(
                    !name.is_empty()
                        && name.len() <= 250
                        && !matches!(name.as_str(), "." | "..")
                        && !name.contains(['/', '\\', '\0']),
                    "Invalid profile backup name"
                );
                parent(&self.config)
                    .join("profiles")
                    .join(format!("{name}.json"))
            }
        })
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("Opening backup directory")
}

fn pinned_path(directory: &File, filename: &std::ffi::OsStr) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(filename)
}

fn open_backup(path: &Path) -> Result<File> {
    let directory = directory(parent(path))?;
    let path = pinned_path(
        &directory,
        path.file_name().context("Backup has no filename")?,
    );
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("Opening backup")
}

pub fn list(locations: &Locations) -> Result<Vec<BackupEntry>> {
    let mut targets: Vec<_> = [
        BackupTarget::Configuration,
        BackupTarget::Templates,
        BackupTarget::RgbPresets,
    ]
    .into_iter()
    .flat_map(|target| [(target.clone(), false), (target, true)])
    .collect();
    let profiles = parent(&locations.config).join("profiles");
    match directory(&profiles) {
        Ok(directory) => {
            let path = format!("/proc/self/fd/{}", directory.as_raw_fd());
            for (index, entry) in fs::read_dir(path)?.enumerate() {
                ensure!(
                    index < 512,
                    "Profile directory exceeds the 512-entry discovery limit"
                );
                let entry = entry?;
                let filename = entry.file_name();
                if let Some((name, preserved)) = filename.to_str().and_then(|name| {
                    name.strip_suffix(".json.bak")
                        .map(|name| (name, false))
                        .or_else(|| {
                            name.strip_suffix(".json.before-restore")
                                .map(|name| (name, true))
                        })
                }) {
                    targets.push((BackupTarget::Profile { name: name.into() }, preserved));
                    ensure!(targets.len() <= 262, "Too many profile backup files");
                }
            }
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let mut entries = Vec::new();
    for (target, preserved) in targets {
        let path = crate::persistence::state_backup_path(&locations.original(&target)?, preserved);
        match open_backup(&path) {
            Ok(file) => {
                let metadata = file.metadata()?;
                entries.push(BackupEntry {
                    target,
                    preserved,
                    bytes: metadata.len(),
                    modified_unix_seconds: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs()),
                    error: if !metadata.is_file() {
                        Some("Backup is not a regular file".into())
                    } else if metadata.len() > MAX_BYTES as u64 {
                        Some("Backup exceeds 16 MiB".into())
                    } else {
                        None
                    },
                });
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => entries.push(BackupEntry {
                target,
                preserved,
                bytes: 0,
                modified_unix_seconds: None,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    entries.sort_by_key(|entry| serde_json::to_string(&entry.target).unwrap_or_default());
    Ok(entries)
}

pub fn preview(locations: &Locations, target: BackupTarget) -> Result<BackupPreview> {
    preview_record(locations, target, false)
}

pub fn preview_record(
    locations: &Locations,
    target: BackupTarget,
    preserved: bool,
) -> Result<BackupPreview> {
    let path = crate::persistence::state_backup_path(&locations.original(&target)?, preserved);
    let pinned = open_backup(&path)?;
    let metadata = pinned.metadata()?;
    ensure!(
        metadata.is_file(),
        "Backup must be a regular file, not a symlink or special file"
    );
    ensure!(metadata.len() <= MAX_BYTES as u64, "Backup exceeds 16 MiB");
    let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "Backup exceeds 16 MiB");
    let mut parser = serde_json::Deserializer::from_slice(&bytes);
    let mut parse_error = serde::de::IgnoredAny::deserialize(&mut parser)
        .and_then(|_| parser.end())
        .err()
        .map(|error| error.to_string());
    let json = match std::str::from_utf8(&bytes) {
        Ok(json) => std::borrow::Cow::Borrowed(json),
        Err(_) => {
            parse_error = Some(
                "Backup is not UTF-8. Invalid text is shown with replacement characters".into(),
            );
            String::from_utf8_lossy(&bytes[..bytes.len().min(PREVIEW_BYTES)])
        }
    };
    let mut end = json.len().min(PREVIEW_BYTES);
    while !json.is_char_boundary(end) {
        end -= 1;
    }
    let validation = validate(locations, &target, &bytes);
    let (warnings, validation_error) = match validation {
        Ok(mut warnings) => {
            if warnings.len() > 64 {
                warnings.truncate(63);
                warnings.push("Additional configuration warnings omitted".into());
            }
            for warning in &mut warnings {
                let mut end = warning.len().min(2048);
                while !warning.is_char_boundary(end) {
                    end -= 1;
                }
                warning.truncate(end);
            }
            (warnings, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    Ok(BackupPreview {
        target,
        preserved,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes: bytes.len(),
        json: json[..end].into(),
        truncated: bytes.len() > PREVIEW_BYTES || end < json.len(),
        parse_error,
        validation_error,
        warnings,
    })
}

pub fn delete(
    locations: &Locations,
    target: BackupTarget,
    preserved: bool,
    sha256: &str,
) -> Result<()> {
    crate::persistence::delete_backup(&locations.original(&target)?, preserved, sha256)
}

pub fn restore(locations: &Locations, target: BackupTarget, sha256: &str) -> Result<()> {
    let original = locations.original(&target)?;
    let backup = crate::persistence::backup_path(&original);
    let bytes = read_bytes(&backup)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == sha256,
        "Backup changed after preview. Review it again"
    );
    validate(locations, &target, &bytes)?;
    let current = match read_bytes(&original) {
        Ok(bytes) => Some(bytes),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    crate::persistence::restore_json(&original, &bytes, current.as_deref())
}

pub(crate) fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = open_backup(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "State must be a regular file");
    ensure!(metadata.len() <= MAX_BYTES as u64, "State exceeds 16 MiB");
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "State exceeds 16 MiB");
    Ok(bytes)
}

pub(crate) fn validate(
    locations: &Locations,
    target: &BackupTarget,
    bytes: &[u8],
) -> Result<Vec<String>> {
    match target {
        BackupTarget::Configuration => {
            let (_, warnings) =
                lianli_shared::config::AppConfig::from_reader(bytes, &locations.config)?;
            return Ok(warnings);
        }
        BackupTarget::Templates => {
            crate::template_store::parse_user_templates(bytes)?;
        }
        BackupTarget::RgbPresets => {
            serde_json::from_slice::<Vec<lianli_shared::rgb::RgbPreset>>(bytes)?;
        }
        BackupTarget::Profile { name } => {
            let profile: lianli_shared::profile::DeviceProfile = serde_json::from_slice(bytes)?;
            ensure!(
                profile.schema_version == 1,
                "Unsupported profile schema version"
            );
            ensure!(
                profile.name == *name,
                "Profile name does not match its backup filename"
            );
            for lcd in profile.lcds {
                lcd.validate_settings()?;
            }
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn locations(root: &Path) -> Locations {
        Locations {
            config: root.join("custom.json"),
            templates: root.join("lcd_templates.json"),
            presets: root.join("rgb_presets.json"),
        }
    }

    #[test]
    fn discovery_is_limited_to_known_sibling_and_profile_backups() {
        let root = tempfile::tempdir().unwrap();
        let locations = locations(root.path());
        assert!(list(&locations).unwrap().is_empty());
        fs::write(root.path().join("custom.json.bak"), "{\"version\":1}").unwrap();
        fs::write(root.path().join("unrelated.json.bak"), "{}").unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        fs::write(root.path().join("profiles/Quiet gaming.json.bak"), "{}").unwrap();
        let entries = list(&locations).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.target
            == BackupTarget::Profile {
                name: "Quiet gaming".into()
            }));
        let first = preview(&locations, BackupTarget::Configuration).unwrap();
        assert_eq!(first.json, "{\"version\":1}");
        assert!(!first.truncated);
        assert!(first.parse_error.is_none());
        fs::write(root.path().join("custom.json.bak"), "broken JSON").unwrap();
        let changed = preview(&locations, BackupTarget::Configuration).unwrap();
        assert_ne!(first.sha256, changed.sha256);
        assert!(changed.parse_error.is_some());
        assert!(preview(
            &locations,
            BackupTarget::Profile {
                name: "../custom".into()
            }
        )
        .is_err());
    }

    #[test]
    fn preview_rejects_symlinks_special_files_and_oversize_before_reading() {
        let root = tempfile::tempdir().unwrap();
        let locations = locations(root.path());
        let backup = root.path().join("custom.json.bak");
        fs::write(root.path().join("private"), "secret").unwrap();
        symlink(root.path().join("private"), &backup).unwrap();
        assert!(list(&locations).unwrap()[0].error.is_some());
        assert!(preview(&locations, BackupTarget::Configuration).is_err());
        fs::remove_file(&backup).unwrap();
        let filename = std::ffi::CString::new(backup.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(filename.as_ptr(), 0o600) }, 0);
        assert!(preview(&locations, BackupTarget::Configuration).is_err());
        fs::remove_file(&backup).unwrap();
        File::create(&backup)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
        assert!(preview(&locations, BackupTarget::Configuration).is_err());
    }

    #[test]
    fn preserved_backup_cleanup_checks_review_and_leaves_active_state_and_other_copies() {
        let root = tempfile::tempdir().unwrap();
        let locations = locations(root.path());
        fs::write(&locations.config, b"{}").unwrap();
        let backup = crate::persistence::backup_path(&locations.config);
        let preserved = crate::persistence::state_backup_path(&locations.config, true);
        fs::write(&backup, b"[]").unwrap();
        fs::write(&preserved, [0xff, b'{']).unwrap();
        let entries = list(&locations).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.preserved));
        let review = preview_record(&locations, BackupTarget::Configuration, true).unwrap();
        assert!(review.parse_error.unwrap().contains("UTF-8"));
        fs::write(&preserved, b"changed").unwrap();
        assert!(delete(
            &locations,
            BackupTarget::Configuration,
            true,
            &review.sha256
        )
        .is_err());
        assert_eq!(fs::read(&preserved).unwrap(), b"changed");
        let review = preview_record(&locations, BackupTarget::Configuration, true).unwrap();
        delete(
            &locations,
            BackupTarget::Configuration,
            true,
            &review.sha256,
        )
        .unwrap();
        assert!(!preserved.exists());
        assert_eq!(fs::read(&locations.config).unwrap(), b"{}");
        assert_eq!(fs::read(&backup).unwrap(), b"[]");
        let review = preview(&locations, BackupTarget::Configuration).unwrap();
        delete(
            &locations,
            BackupTarget::Configuration,
            false,
            &review.sha256,
        )
        .unwrap();
        assert!(list(&locations).unwrap().is_empty());
    }

    #[test]
    fn restore_uses_reviewed_bytes_and_rejects_replaced_backups() {
        let root = tempfile::tempdir().unwrap();
        let locations = locations(root.path());
        let backup = crate::persistence::backup_path(&locations.config);
        fs::write(&backup, b"{}").unwrap();
        let review = preview(&locations, BackupTarget::Configuration).unwrap();
        fs::write(&backup, b"{\"default_fps\":20}").unwrap();
        assert!(restore(&locations, BackupTarget::Configuration, &review.sha256).is_err());
        assert!(!locations.config.exists());
        let review = preview(&locations, BackupTarget::Configuration).unwrap();
        restore(&locations, BackupTarget::Configuration, &review.sha256).unwrap();
        assert_eq!(
            fs::read(&locations.config).unwrap(),
            fs::read(&backup).unwrap()
        );
    }

    #[test]
    fn preview_distinguishes_json_syntax_from_state_schema_and_load_warnings() {
        let root = tempfile::tempdir().unwrap();
        let locations = locations(root.path());
        let path = root.path().join("custom.json.bak");
        fs::write(&path, r#"{"default_fps":0}"#).unwrap();
        let result = preview(&locations, BackupTarget::Configuration).unwrap();
        assert!(result.parse_error.is_none());
        assert!(result.validation_error.unwrap().contains("default_fps"));
        fs::write(&path, r#"{"lcds":[{"type":"color","rgb":[0,0,0]}]}"#).unwrap();
        let result = preview(&locations, BackupTarget::Configuration).unwrap();
        assert!(result.validation_error.is_none());
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.contains("missing both")));
        fs::create_dir(root.path().join("profiles")).unwrap();
        fs::write(
            root.path().join("profiles/Quiet.json.bak"),
            r#"{"name":"Another","device_id":"test"}"#,
        )
        .unwrap();
        let result = preview(
            &locations,
            BackupTarget::Profile {
                name: "Quiet".into(),
            },
        )
        .unwrap();
        assert!(result.parse_error.is_none());
        assert!(result.validation_error.unwrap().contains("filename"));
        assert!(!locations.config.exists());
    }

    #[test]
    fn long_unicode_preview_is_bounded_and_keeps_the_full_backup_untouched() {
        let root = tempfile::tempdir().unwrap();
        let locations = locations(root.path());
        let json = serde_json::to_string(&"界".repeat(30_000)).unwrap();
        let path = root.path().join("custom.json.bak");
        fs::write(&path, &json).unwrap();
        let result = preview(&locations, BackupTarget::Configuration).unwrap();
        assert!(result.truncated);
        assert!(result.json.len() <= PREVIEW_BYTES);
        assert!(result.parse_error.is_none());
        assert_eq!(fs::read_to_string(path).unwrap(), json);
    }
}
