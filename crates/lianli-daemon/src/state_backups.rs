use anyhow::{ensure, Context, Result};
use lianli_shared::backups::BackupTarget;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct Locations {
    pub config: PathBuf,
    pub templates: PathBuf,
    pub presets: PathBuf,
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
