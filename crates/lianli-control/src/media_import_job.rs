use crate::media_import::PublishedSelection;
use crate::media_publication::Directory;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const LIMIT: usize = 17 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    version: u32,
    pub id: String,
    pub active: bool,
    pub result: Option<PublishedSelection>,
    pub error: Option<String>,
}

struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub struct Job {
    directory: Directory,
    _lock: Lock,
    id: String,
}

fn directory(path: &Path, create: bool) -> Result<Directory> {
    if create {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {
                File::open(path.parent().context("Missing import-job parent")?)?.sync_all()?
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let directory = Directory::open(path)?;
    ensure!(
        directory.0.metadata()?.mode() & 0o077 == 0,
        "Import job directory must be private"
    );
    Ok(directory)
}

fn file(directory: &Directory, name: &str, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(directory.path().join(name))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1
            && metadata.len() <= LIMIT as u64,
        "Import job file has unsafe ownership, type, links or size"
    );
    Ok(file)
}

fn try_lock(file: &File, mode: i32) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error.into())
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn write(directory: &Directory, status: &Status) -> Result<()> {
    let bytes = serde_json::to_vec(status)?;
    ensure!(bytes.len() <= LIMIT, "Import job result exceeds 17 MiB");
    let mut staged = tempfile::NamedTempFile::new_in(directory.path())?;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(directory.path().join("status.json"))?;
    directory.0.sync_all().context("Syncing import job status")
}

impl Job {
    pub fn begin(path: &Path, id: &str) -> Result<Self> {
        ensure!(valid_id(id), "Invalid import job identity");
        let directory = directory(path, true)?;
        let lock = file(&directory, "worker.lock", true)?;
        ensure!(
            try_lock(&lock, libc::LOCK_EX)?,
            "A managed import worker is already active"
        );
        let job = Self {
            directory,
            _lock: Lock(lock),
            id: id.into(),
        };
        write(
            &job.directory,
            &Status {
                version: 1,
                id: id.into(),
                active: true,
                result: None,
                error: None,
            },
        )?;
        Ok(job)
    }

    pub fn finish(self, result: Result<PublishedSelection>) -> Result<()> {
        let mut status = Status {
            version: 1,
            id: self.id.clone(),
            active: false,
            result: None,
            error: None,
        };
        match result {
            Ok(result) if result.import_id == self.id => status.result = Some(result),
            Ok(_) => {
                status.error = Some(
                    "Import helper returned a different identity. Inspect storage before retrying"
                        .into(),
                )
            }
            Err(error) => status.error = Some(format!("{error:#}").chars().take(2048).collect()),
        }
        write(&self.directory, &status)
    }
}

pub fn read(path: &Path) -> Result<Option<Status>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let directory = directory(path, false)?;
    let lock = match file(&directory, "worker.lock", false) {
        Ok(lock) => lock,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let inactive = try_lock(&lock, libc::LOCK_SH)?;
    let _observation = if inactive { Some(Lock(lock)) } else { None };
    let mut bytes = Vec::new();
    file(&directory, "status.json", false)?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= LIMIT,
        "Import job status grew beyond its limit"
    );
    let mut status: Status = serde_json::from_slice(&bytes)?;
    if let Some(result) = &status.result {
        lianli_shared::media_dependencies::validate_dependency_input(
            &result.lcds,
            &result.templates,
        )
        .map_err(anyhow::Error::msg)?;
    }
    ensure!(
        status.version == 1
            && valid_id(&status.id)
            && (if status.active {
                status.result.is_none() && status.error.is_none()
            } else {
                status.result.is_some() != status.error.is_some()
            })
            && status
                .result
                .as_ref()
                .is_none_or(|result| result.import_id == status.id)
            && status
                .error
                .as_ref()
                .is_none_or(|error| error.chars().count() <= 2048),
        "Invalid import job status"
    );
    if inactive {
        if status.active {
            status.active = false;
            status.error = Some("Import worker stopped. Files may have been copied. Inspect storage before retrying.".into());
        }
    } else {
        status.active = true;
        status.result = None;
        status.error = None;
    }
    Ok(Some(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "1234567890abcdef1234567890abcdef";

    #[test]
    fn live_lock_excludes_duplicate_jobs_and_abandoned_status_never_claims_success() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("job");
        assert!(read(&path).unwrap().is_none());
        fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("selection-pending.json"), b"[]").unwrap();
        assert!(read(&path).unwrap().is_none());
        let job = Job::begin(&path, ID).unwrap();
        assert!(read(&path).unwrap().unwrap().active);
        assert!(Job::begin(&path, ID).is_err());
        drop(job);
        let stopped = read(&path).unwrap().unwrap();
        assert!(!stopped.active && stopped.result.is_none());
        assert!(stopped
            .error
            .unwrap()
            .contains("Files may have been copied"));
        let job = Job::begin(&path, ID).unwrap();
        job.finish(Err(anyhow::anyhow!("x".repeat(4096)))).unwrap();
        assert_eq!(read(&path).unwrap().unwrap().error.unwrap().len(), 2048);
    }

    #[test]
    fn terminal_result_survives_reopen_and_status_links_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("job");
        Job::begin(&path, ID)
            .unwrap()
            .finish(Ok(PublishedSelection {
                import_id: ID.into(),
                lcds: vec![],
                templates: vec![],
                destination: None,
            }))
            .unwrap();
        let completed = read(&path).unwrap().unwrap();
        assert!(!completed.active && completed.error.is_none());
        assert_eq!(completed.result.unwrap().import_id, ID);
        fs::rename(path.join("status.json"), path.join("saved.json")).unwrap();
        std::os::unix::fs::symlink("saved.json", path.join("status.json")).unwrap();
        assert!(read(&path).is_err());
    }
}
