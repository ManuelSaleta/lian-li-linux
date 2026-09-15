use anyhow::{bail, Context, Result};
use lianli_shared::installation::InstallationContext;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use tracing::{info, warn};

pub struct PidLock {
    file: File,
}

#[derive(Debug)]
enum LockFailure {
    HeldByAnother(String),
    Unopenable(anyhow::Error),
}

impl PidLock {
    pub fn acquire() -> Result<Self> {
        let context = InstallationContext::detect();
        let Some(path) = context.daemon_lock_path() else {
            bail!("Cannot verify host-wide daemon ownership from this container. Use the documented Distrobox host integration: https://github.com/sgtaziz/lian-li-linux/blob/main/docs/service-modes.md");
        };
        match lock_pidfile(&path) {
            Ok(file) => {
                info!("Acquired shared daemon lock at {}", path.display());
                Ok(Self { file })
            }
            Err(LockFailure::HeldByAnother(pid)) => bail!(
                "Another lianli-daemon holds {} (reported PID {}). Stop the active daemon before switching service modes.",
                path.display(), if pid.is_empty() { "unknown" } else { &pid }
            ),
            Err(LockFailure::Unopenable(error)) => Err(error).with_context(|| format!(
                "Shared daemon lock {} is unavailable; refusing to start a competing hardware owner. Install the tmpfiles rule on the host and run `sudo systemd-tmpfiles --create lianli.conf`. See https://github.com/sgtaziz/lian-li-linux/blob/main/docs/service-modes.md",
                path.display()
            )),
        }
    }

    pub fn identity(&self) -> Result<lianli_shared::daemon::FileIdentity> {
        let metadata = self.file.metadata()?;
        Ok(lianli_shared::daemon::FileIdentity {
            device: metadata.dev().to_string(),
            inode: metadata.ino().to_string(),
        })
    }
}

fn lock_pidfile(path: &Path) -> std::result::Result<File, LockFailure> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| {
            LockFailure::Unopenable(
                anyhow::Error::from(e).context(format!("opening {}", path.display())),
            )
        })?;

    let metadata = file
        .metadata()
        .map_err(|error| LockFailure::Unopenable(error.into()))?;
    if !metadata.is_file() {
        return Err(LockFailure::Unopenable(anyhow::anyhow!(
            "daemon lock is not a regular file"
        )));
    }

    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        if errno.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let mut existing = String::new();
            let _ = (&mut file).take(32).read_to_string(&mut existing);
            let pid = existing.trim();
            let pid = if pid.bytes().all(|byte| byte.is_ascii_digit()) {
                pid
            } else {
                "unknown"
            };
            return Err(LockFailure::HeldByAnother(pid.to_string()));
        }
        return Err(LockFailure::Unopenable(
            anyhow::Error::from(errno).context(format!("flock {}", path.display())),
        ));
    }

    let mut record_pid = || -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(())
    };
    if let Err(error) = record_pid() {
        warn!("Daemon lock acquired, but recording its PID failed: {error}");
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_identity_tracks_the_held_file_after_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        std::fs::write(&path, "").unwrap();
        let held = PidLock {
            file: lock_pidfile(&path).unwrap(),
        };
        let original = held.identity().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "123").unwrap();
        assert_eq!(held.identity().unwrap(), original);
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino().to_string(),
            original.inode
        );
    }

    #[test]
    fn independent_opens_cannot_hold_the_same_lock() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let first = lock_pidfile(file.path()).unwrap();
        assert!(matches!(
            lock_pidfile(file.path()),
            Err(LockFailure::HeldByAnother(_))
        ));
        drop(first);
        assert!(lock_pidfile(file.path()).is_ok());
    }

    #[test]
    fn missing_and_symlinked_locks_are_rejected_without_creating_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        assert!(matches!(
            lock_pidfile(&path),
            Err(LockFailure::Unopenable(_))
        ));
        assert!(!path.exists());
        let actual = dir.path().join("actual");
        std::fs::write(&actual, "preserve").unwrap();
        std::os::unix::fs::symlink(&actual, &path).unwrap();
        assert!(matches!(
            lock_pidfile(&path),
            Err(LockFailure::Unopenable(_))
        ));
        assert_eq!(std::fs::read_to_string(actual).unwrap(), "preserve");
    }
}
