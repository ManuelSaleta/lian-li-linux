use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

const SHARED_LOCK: &str = "/run/lianli-daemon.lock";

pub struct PidLock {
    _file: File,
}

enum LockFailure {
    HeldByAnother(String),
    Unopenable(anyhow::Error),
}

impl PidLock {
    pub fn acquire(system: bool) -> Result<Self> {
        match lock_pidfile(Path::new(SHARED_LOCK)) {
            Ok(file) => {
                info!("Acquired shared pidlock at {SHARED_LOCK}");
                return Ok(Self { _file: file });
            }
            Err(LockFailure::HeldByAnother(pid)) => {
                error!(
                    "Another lianli-daemon already holds {SHARED_LOCK} (pid={}). \
                     Refusing to start.",
                    if pid.is_empty() { "?" } else { &pid }
                );
                std::process::exit(1);
            }
            Err(LockFailure::Unopenable(e)) => {
                warn!("shared lock {SHARED_LOCK} unavailable ({e}), cross-mode mutex disabled");
            }
        }

        let mut last_err: Option<anyhow::Error> = None;
        for path in candidate_paths(system) {
            match lock_pidfile(&path) {
                Ok(file) => {
                    info!("Acquired pidlock at {}", path.display());
                    return Ok(Self { _file: file });
                }
                Err(LockFailure::HeldByAnother(pid)) => {
                    error!(
                        "Another lianli-daemon already holds {} (pid={}). Refusing to start.",
                        path.display(),
                        if pid.is_empty() { "?" } else { &pid }
                    );
                    std::process::exit(1);
                }
                Err(LockFailure::Unopenable(e)) => {
                    debug!("pidlock candidate {} unavailable: {e}", path.display());
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no pidlock candidate writable")))
    }
}

fn candidate_paths(system: bool) -> Vec<PathBuf> {
    if system {
        return vec![PathBuf::from("/run/lianli/lianli-daemon.pid")];
    }
    let mut paths = vec![
        PathBuf::from("/run/lianli-daemon.pid"),
        PathBuf::from("/var/run/lianli-daemon.pid"),
    ];
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        paths.push(PathBuf::from(xdg).join("lianli-daemon.pid"));
    }
    paths
}

fn lock_pidfile(path: &Path) -> std::result::Result<File, LockFailure> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            LockFailure::Unopenable(
                anyhow::Error::from(e).context(format!("opening {}", path.display())),
            )
        })?;

    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        if errno.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let mut existing = String::new();
            let _ = file.seek(SeekFrom::Start(0));
            let _ = file.read_to_string(&mut existing);
            return Err(LockFailure::HeldByAnother(existing.trim().to_string()));
        }
        return Err(LockFailure::Unopenable(
            anyhow::Error::from(errno).context(format!("flock {}", path.display())),
        ));
    }

    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.set_len(0);
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.sync_all();
    Ok(file)
}
