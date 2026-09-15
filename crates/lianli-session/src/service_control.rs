use anyhow::{ensure, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Identity {
    invocation: String,
    pid: u32,
    started: u64,
}

fn process_start(pid: u32) -> Result<u64> {
    let mut text = String::new();
    File::open(format!("/proc/{pid}/stat"))?
        .take(4097)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() <= 4096,
        "Capture process identity exceeds its size limit"
    );
    let (number, rest) = text
        .split_once(" (")
        .context("Invalid capture process identity")?;
    ensure!(
        number.parse::<u32>()? == pid,
        "Capture process PID mismatch"
    );
    // The process name may contain spaces and parentheses.
    rest.rsplit_once(") ")
        .context("Invalid capture process name")?
        .1
        .split_whitespace()
        .nth(19)
        .context("Missing capture process start time")?
        .parse()
        .context("Invalid capture process start time")
}

fn runtime() -> PathBuf {
    PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() }))
}

fn open(runtime: &Path, create: bool) -> Result<File> {
    let uid = unsafe { libc::geteuid() };
    let directory = fs::metadata(runtime)?;
    ensure!(
        uid != 0 && directory.is_dir() && directory.uid() == uid && directory.mode() & 0o077 == 0,
        "Capture service needs a private desktop-user runtime directory"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(runtime.join("lianli-session-service.lock"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == uid
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= 256,
        "Invalid capture service ownership record"
    );
    Ok(file)
}

fn read(file: &mut File) -> Result<Option<Identity>> {
    file.rewind()?;
    let mut text = String::new();
    file.take(257).read_to_string(&mut text)?;
    ensure!(text.len() <= 256, "Capture ownership record is too large");
    if text.is_empty() {
        return Ok(None);
    }
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure!(fields.len() == 3, "Incomplete capture ownership record");
    let invocation =
        lianli_shared::daemon::parse_service_invocation(fields[0]).map_err(anyhow::Error::msg)?;
    let pid = fields[1].parse::<u32>()?;
    ensure!(pid > 0, "Invalid capture owner PID");
    Ok(Some(Identity {
        invocation,
        pid,
        started: fields[2].parse()?,
    }))
}

pub struct Guard {
    _file: File,
}

impl Guard {
    pub fn start(invocation: &str) -> Result<Self> {
        Self::at(&runtime(), invocation)
    }

    fn at(runtime: &Path, invocation: &str) -> Result<Self> {
        lianli_shared::daemon::parse_service_invocation(invocation).map_err(anyhow::Error::msg)?;
        let mut file = open(runtime, true)?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Another capture service owns the runtime record"
        );
        let pid = std::process::id();
        let started = process_start(pid)?;
        if let Some(previous) = read(&mut file)? {
            match process_start(previous.pid) {
                Ok(current) => {
                    ensure!(
                        current != previous.started
                            || (previous.pid == pid
                                && previous.started == started
                                && previous.invocation == invocation),
                        "A previous capture service is still running"
                    );
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                Err(error) => {
                    return Err(error).context("Cannot verify the previous capture service")
                }
            }
        }
        file.rewind()?;
        file.set_len(0)?;
        // Retain the last identity so ExecStop can verify an already-exited invocation.
        writeln!(file, "{invocation} {pid} {started}")?;
        file.flush()?;
        Ok(Self { _file: file })
    }
}

pub fn stop(invocation: &str) -> Result<()> {
    stop_at(&runtime(), invocation)
}

fn stop_at(runtime: &Path, invocation: &str) -> Result<()> {
    lianli_shared::daemon::parse_service_invocation(invocation).map_err(anyhow::Error::msg)?;
    let mut file = open(runtime, false)
        .context("Capture service has not registered; shutdown cannot be verified")?;
    let identity = read(&mut file)?
        .context("Capture service is still registering; shutdown cannot be verified")?;
    ensure!(
        identity.invocation == invocation,
        "Capture service invocation changed; refusing to stop another instance"
    );
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) } as i32;
    if raw < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error).context("Opening capture service process handle");
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw) };
    ensure!(
        process_start(identity.pid)? == identity.started
            && fs::metadata(format!("/proc/{}", identity.pid))?.uid() == unsafe { libc::geteuid() },
        "Capture service process identity changed"
    );
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("Requesting capture shutdown");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 250) };
        if result > 0 && poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(());
        }
        ensure!(
            poll.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
            "Capture process handle failed while waiting for shutdown"
        );
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error()).context("Waiting for capture shutdown");
        }
        ensure!(
            Instant::now() < deadline,
            "Capture service did not exit within 25 seconds; no forced signal was sent"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    const INVOCATION: &str = "0123456789abcdef0123456789abcdef";

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/support/unprivileged.rs"
    ));

    #[test]
    fn ownership_refuses_other_invocations_and_reused_process_identities() {
        if run_as_desktop_user("service_control::tests::ownership_refuses_other_invocations_and_reused_process_identities")
        {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut guard = Guard::at(root.path(), INVOCATION).unwrap();
        assert!(Guard::at(root.path(), INVOCATION).is_err());
        assert!(stop_at(root.path(), "abcdef0123456789abcdef0123456789").is_err());
        guard._file.rewind().unwrap();
        guard._file.set_len(0).unwrap();
        writeln!(guard._file, "{INVOCATION} {} 0", std::process::id()).unwrap();
        guard._file.flush().unwrap();
        assert!(stop_at(root.path(), INVOCATION)
            .unwrap_err()
            .to_string()
            .contains("identity changed"));
        drop(guard);
        assert!(root.path().join("lianli-session-service.lock").exists());
    }

    #[test]
    fn stop_waits_for_only_the_registered_fixture_process() {
        if run_as_desktop_user(
            "service_control::tests::stop_waits_for_only_the_registered_fixture_process",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "service_control::tests::capture_service_fixture",
            ])
            .env("LIANLI_TEST_CAPTURE_SERVICE_RUNTIME", root.path())
            .env_remove("LIANLI_TEST_CAPTURE_SERVICE_EXECUTED")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let ready = loop {
            if root.path().join("ready").exists()
                && open(root.path(), false)
                    .ok()
                    .and_then(|mut file| read(&mut file).ok().flatten())
                    .is_some()
            {
                break true;
            }
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let result = if ready {
            stop_at(root.path(), INVOCATION)
        } else {
            Err(anyhow::anyhow!("Fixture did not register"))
        };
        if result.is_err() {
            child.kill().unwrap();
        }
        let status = child.wait().unwrap();
        result.unwrap();
        assert!(!status.success());
        stop_at(root.path(), INVOCATION).unwrap();
        let guard = Guard::at(root.path(), INVOCATION).unwrap();
        drop(guard);
    }

    #[test]
    #[ignore = "Private process fixture launched only by its parent test"]
    fn capture_service_fixture() {
        use std::os::unix::process::CommandExt;
        let runtime = std::env::var_os("LIANLI_TEST_CAPTURE_SERVICE_RUNTIME")
            .expect("private fixture runtime");
        let _guard = Guard::at(Path::new(&runtime), INVOCATION).unwrap();
        if std::env::var_os("LIANLI_TEST_CAPTURE_SERVICE_EXECUTED").is_none() {
            let error = std::process::Command::new(std::env::current_exe().unwrap())
                .args(std::env::args_os().skip(1))
                .env("LIANLI_TEST_CAPTURE_SERVICE_EXECUTED", "1")
                .exec();
            panic!("Fixture exec failed: {error}");
        }
        fs::write(Path::new(&runtime).join("ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }
}
