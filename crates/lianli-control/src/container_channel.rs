use crate::transfer_channel::Channel;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use socket2::{Domain, SockAddr, Socket, Type};
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Serialize, Deserialize)]
enum Message {
    Hello { token: String },
    Channel,
    Stdout,
    Stderr,
    Ready,
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        path.is_absolute()
            && metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Container transfer requires a private runtime directory owned by this account"
    );
    Ok(())
}

fn verify_peer(socket: &Socket) -> Result<()> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0
            && length as usize == std::mem::size_of_val(&credentials)
            && credentials.uid == unsafe { libc::geteuid() }
            && credentials.pid > 0,
        "Container transfer peer belongs to another account or an invisible process"
    );
    Ok(())
}

fn wait(fd: BorrowedFd<'_>, events: i16, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Container channel handoff timed out");
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let result =
            unsafe { libc::poll(&mut poll, 1, remaining.as_millis().clamp(1, 30_000) as i32) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error()).context("Waiting for container channel");
        }
    }
}

struct Broker {
    directory: tempfile::TempDir,
    listener: Socket,
    token: String,
}

impl Broker {
    fn new(runtime: &Path) -> Result<Self> {
        private_directory(runtime)?;
        let directory = tempfile::Builder::new()
            .prefix("lianli-transfer-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(runtime)?;
        let listener = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
        listener.set_cloexec(true)?;
        listener.set_nonblocking(true)?;
        listener.bind(&SockAddr::unix(directory.path().join("channel"))?)?;
        listener.listen(1)?;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self {
            directory,
            listener,
            token,
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join("channel")
    }

    fn handoff(&self, input: BorrowedFd<'_>, timeout: Duration) -> Result<Channel> {
        let deadline = Instant::now() + timeout;
        let socket = loop {
            match self.listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    wait(self.listener.as_fd(), libc::POLLIN, deadline)?
                }
                Err(error) => return Err(error).context("Accepting container worker"),
            }
        };
        verify_peer(&socket)?;
        let channel = Channel::new(
            socket.into(),
            deadline.saturating_duration_since(Instant::now()),
        )?;
        let (hello, descriptor): (Message, _) = channel.receive()?;
        ensure!(
            descriptor.is_none()
                && matches!(hello, Message::Hello { token } if token == self.token),
            "Container worker handoff did not match its operation"
        );
        channel.send(&Message::Channel, Some(input))?;
        channel.send(&Message::Stdout, Some(std::io::stdout().as_fd()))?;
        channel.send(&Message::Stderr, Some(std::io::stderr().as_fd()))?;
        let (ready, descriptor): (Message, _) = channel.receive()?;
        ensure!(
            descriptor.is_none() && matches!(ready, Message::Ready),
            "Container worker did not acknowledge its channel"
        );
        Ok(channel)
    }
}

pub fn run(
    name: &str,
    enter: &Path,
    binaries: &Path,
    arguments: &[OsString],
    destination: Option<&str>,
) -> Result<crate::command::Output> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Enter container workers as the unprivileged box owner"
    );
    ensure!(
        crate::distrobox_unit::valid_name(name)
            && enter.is_absolute()
            && enter
                .file_name()
                .is_some_and(|value| value == "distrobox-enter")
            && binaries.is_absolute(),
        "Invalid container worker launch route"
    );
    ensure!(
        !arguments.is_empty()
            && arguments.len() <= 32
            && arguments.iter().all(|value| value.len() <= 4096),
        "Invalid container worker arguments"
    );
    ensure!(
        arguments[0].to_str().is_some_and(worker_command),
        "Only state transfer workers may receive a container channel"
    );
    let runtime = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").context("Missing host user runtime directory")?,
    );
    let broker = Broker::new(&runtime)?;
    let input = std::io::stdin().as_fd().try_clone_to_owned()?;
    if arguments[0].to_str().is_some_and(packet_worker) {
        Channel::new(input.as_fd().try_clone_to_owned()?, WORKER_TIMEOUT)?;
    }
    let mut command = Command::new(enter);
    command
        .env_remove("INVOCATION_ID")
        .args(["--no-tty", "--name", name, "--"])
        .arg(binaries.join("lianli-control"))
        .arg("--transfer-channel")
        .arg(broker.path())
        .arg("--transfer-token")
        .arg(&broker.token);
    if let Some(destination) = destination {
        ensure!(
            destination.len() <= 16 * 1024,
            "Container destination metadata exceeds 16 KiB"
        );
        command.arg("--worker-destination").arg(destination);
    }
    command.args(arguments);
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let worker =
            scope.spawn(|| crate::command::run_cancelable(command, WORKER_TIMEOUT, &cancel));
        let handoff = broker.handoff(input.as_fd(), HANDOFF_TIMEOUT);
        if handoff.is_err() {
            cancel.store(true, Ordering::Release);
        }
        let result = worker
            .join()
            .map_err(|_| anyhow::anyhow!("Container launch worker panicked"))?;
        if handoff.is_err() {
            if let Ok(output) = &result {
                ensure!(
                    output.status.success(),
                    "Container launcher failed before channel handoff: {}",
                    output.stderr.trim()
                );
            }
        }
        handoff.context("Handing the transfer channel to the container")?;
        result.map(|mut output| {
            output.stdout.clear();
            output
        })
    })
}

pub fn worker_command(command: &str) -> bool {
    packet_worker(command)
        || matches!(
            command,
            "inspect-container-destination"
                | "inspect-container-identity"
                | "inspect-destination"
                | "check-saved-state"
                | "check-recovery-access"
                | "discard-transfer"
                | "finish-transfer"
        )
}

pub fn packet_worker(command: &str) -> bool {
    matches!(
        command,
        "send-state" | "receive-state" | "publish-state" | "receive-selected-media"
    )
}

struct Received {
    input: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
    lease: Channel,
}

fn receive(path: &Path, token: &str, packet: bool) -> Result<Received> {
    private_directory(
        path.parent()
            .context("Container channel has no parent directory")?,
    )?;
    lianli_shared::daemon::parse_service_invocation(token).map_err(anyhow::Error::msg)?;
    let socket = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
    socket.set_nonblocking(true)?;
    socket.set_cloexec(true)?;
    if let Err(error) = socket.connect(&SockAddr::unix(path)?) {
        ensure!(
            error.raw_os_error() == Some(libc::EINPROGRESS),
            "Connecting container channel: {error}"
        );
        wait(
            socket.as_fd(),
            libc::POLLOUT,
            Instant::now() + HANDOFF_TIMEOUT,
        )?;
        if let Some(error) = socket.take_error()? {
            return Err(error).context("Connecting container channel");
        }
    }
    verify_peer(&socket)?;
    let channel = Channel::new(socket.into(), HANDOFF_TIMEOUT)?;
    channel.send(
        &Message::Hello {
            token: token.into(),
        },
        None,
    )?;
    let (message, input): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::Channel),
        "Unexpected container handoff response"
    );
    let input = input.context("Missing container transfer descriptor")?;
    if packet {
        Channel::new(input.as_fd().try_clone_to_owned()?, WORKER_TIMEOUT)?;
    }
    let (message, stdout): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::Stdout),
        "Missing container stdout handoff"
    );
    let stdout = stdout.context("Missing container stdout descriptor")?;
    let (message, stderr): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::Stderr),
        "Missing container stderr handoff"
    );
    let stderr = stderr.context("Missing container stderr descriptor")?;
    Ok(Received {
        input,
        stdout,
        stderr,
        lease: channel,
    })
}

pub struct Guard {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Guard {
    fn watch(channel: Channel) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name("container-parent".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let mut fd = libc::pollfd {
                        fd: channel.as_fd().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let result = unsafe { libc::poll(&mut fd, 1, 100) };
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if result > 0
                        || (result < 0
                            && std::io::Error::last_os_error().kind()
                                != std::io::ErrorKind::Interrupted)
                    {
                        // Podman workers do not inherit the host coordinator's parent-death signal.
                        unsafe {
                            libc::_exit(1);
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("Container parent monitor panicked");
            }
        }
    }
}

pub fn attach(path: &Path, token: &str, packet: bool) -> Result<Guard> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Receive container worker channels under an unprivileged account"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0,
        "Cannot restrict container worker privileges: {}",
        std::io::Error::last_os_error()
    );
    let Received {
        input,
        stdout,
        stderr,
        lease,
    } = receive(path, token, packet)?;
    install_descriptor(input, libc::STDIN_FILENO)?;
    install_descriptor(stdout, libc::STDOUT_FILENO)?;
    install_descriptor(stderr, libc::STDERR_FILENO)?;
    lease.send(&Message::Ready, None)?;
    Guard::watch(lease)
}

fn install_descriptor(input: OwnedFd, target: i32) -> Result<()> {
    let descriptor = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(
        descriptor >= 0,
        "Duplicating the container transfer descriptor: {}",
        std::io::Error::last_os_error()
    );
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    drop(input);
    ensure!(
        unsafe { libc::dup2(descriptor.as_raw_fd(), target) } >= 0,
        "Installing the container transfer descriptor: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Stdio;

    fn private_runtime() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn handoff_preserves_prequeued_packets_and_file_descriptors() {
        let runtime = private_runtime();
        let broker = Broker::new(runtime.path()).unwrap();
        let (sender, receiver) = Channel::pair().unwrap();
        let sender = Channel::new(sender, Duration::from_secs(2)).unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"configuration payload").unwrap();
        file.rewind().unwrap();
        sender.send(&"first", Some(file.as_fd())).unwrap();
        sender.send(&"second", None).unwrap();
        drop(file);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let received = receive(&broker.path(), &broker.token, true).unwrap();
                received.lease.send(&Message::Ready, None).unwrap();
                let input = Channel::new(received.input, Duration::from_secs(2)).unwrap();
                let (first, file): (String, _) = input.receive().unwrap();
                assert_eq!(first, "first");
                let mut payload = String::new();
                std::fs::File::from(file.unwrap())
                    .read_to_string(&mut payload)
                    .unwrap();
                assert_eq!(payload, "configuration payload");
                let (second, descriptor): (String, _) = input.receive().unwrap();
                assert_eq!(second, "second");
                assert!(descriptor.is_none());
                input.send(&"confirmed", None).unwrap();
            });
            let lease = broker
                .handoff(receiver.as_fd(), Duration::from_secs(2))
                .unwrap();
            let (reply, descriptor): (String, _) = sender.receive().unwrap();
            assert_eq!(reply, "confirmed");
            assert!(descriptor.is_none());
            worker.join().unwrap();
            drop(lease);
        });
        let path = broker.path();
        drop(broker);
        assert!(!path.exists());
    }

    #[test]
    fn a_wrong_token_receives_no_channel_and_absent_workers_time_out() {
        let runtime = private_runtime();
        let broker = Broker::new(runtime.path()).unwrap();
        let (_parent, child) = Channel::pair().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                receive(&broker.path(), "00000000000000000000000000000001", true).is_err()
            });
            assert!(broker
                .handoff(child.as_fd(), Duration::from_secs(1))
                .is_err());
            assert!(worker.join().unwrap());
        });
        let began = Instant::now();
        assert!(broker
            .handoff(child.as_fd(), Duration::from_millis(10))
            .is_err());
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn channel_locations_must_be_private_and_workers_cannot_control_hardware() {
        let runtime = private_runtime();
        let alias = runtime.path().join("alias");
        symlink(runtime.path(), &alias).unwrap();
        assert!(Broker::new(&alias).is_err());
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Broker::new(runtime.path()).is_err());
        for name in [
            "stop-service",
            "switch-mode",
            "diagnose",
            "box-worker",
            "send-state --help",
        ] {
            assert!(!worker_command(name));
        }
    }

    #[test]
    fn a_container_worker_exits_when_its_host_lease_disappears() {
        if std::env::var_os("LIANLI_CHANNEL_MONITOR_CHILD").is_some() {
            let channel = Channel::new(
                std::io::stdin().as_fd().try_clone_to_owned().unwrap(),
                Duration::from_secs(2),
            )
            .unwrap();
            channel.send(&Message::Ready, None).unwrap();
            let _guard = Guard::watch(channel).unwrap();
            loop {
                std::thread::park();
            }
        }
        let (parent, child) = Channel::pair().unwrap();
        let parent = Channel::new(parent, Duration::from_secs(2)).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "container_channel::tests::a_container_worker_exits_when_its_host_lease_disappears",
                "--exact",
            ])
            .env("LIANLI_CHANNEL_MONITOR_CHILD", "1");
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::command::run_with_stdin(command, Stdio::from(child), Duration::from_secs(3))
            });
            let (ready, descriptor): (Message, _) = parent.receive().unwrap();
            assert!(matches!(ready, Message::Ready) && descriptor.is_none());
            drop(parent);
            let output = worker.join().unwrap().unwrap();
            assert_eq!(output.status.code(), Some(1));
        });
    }

    #[test]
    fn completed_workers_stop_and_join_the_parent_monitor() {
        let (_parent, child) = Channel::pair().unwrap();
        let guard = Guard::watch(Channel::new(child, Duration::from_secs(1)).unwrap()).unwrap();
        let began = Instant::now();
        drop(guard);
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn failed_handoffs_can_cancel_and_reap_the_host_launcher() {
        let cancel = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let began = Instant::now();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::command::run_cancelable(command, Duration::from_secs(30), &cancel)
            });
            std::thread::sleep(Duration::from_millis(25));
            cancel.store(true, Ordering::Release);
            let error = worker.join().unwrap().unwrap_err();
            assert!(error.to_string().contains("cancelled"), "{error:#}");
        });
        assert!(began.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[ignore = "requires explicit control binaries and a local adapter or the hardware-free release VM"]
    fn installed_box_worker_transfers_a_sealed_configuration() {
        let local_adapter = std::env::var_os("LIANLI_CHANNEL_TEST_LOCAL_ADAPTER").is_some();
        if !local_adapter {
            assert_eq!(
                std::fs::read_to_string("/proc/sys/kernel/hostname")
                    .unwrap()
                    .trim(),
                "lianli-release-test"
            );
            assert!(!Path::new("/dev/bus/usb").exists() && !Path::new("/dev/dri").exists());
            assert_eq!(unsafe { libc::geteuid() }, 1000);
        }
        assert_ne!(unsafe { libc::geteuid() }, 0);
        let binaries = PathBuf::from(std::env::var_os("LIANLI_CHANNEL_TEST_BINARIES").unwrap());
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let config = directory.path().join("config.json");
        let settings = serde_json::to_value(lianli_shared::config::AppConfig::default()).unwrap();
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        let snapshot = crate::state::StateSnapshot::read(&config, directory.path()).unwrap();
        let (parent, child) = Channel::pair().unwrap();
        let parent = Channel::new(parent, Duration::from_secs(45)).unwrap();
        let mut command = Command::new(binaries.join("lianli-control"));
        command
            .args(["box-worker", "--box", "lianli-release-box", "--binaries"])
            .arg(&binaries);
        let runtime = private_runtime();
        if local_adapter {
            let enter = runtime.path().join("distrobox-enter");
            std::fs::write(&enter, "#!/bin/sh\nset -eu\ntest \"$1\" = --no-tty\ntest \"$2\" = --name\ntest \"$3\" = lianli-release-box\ntest \"$4\" = --\nshift 4\nprintf 'simulated container startup\\n'\nexec \"$@\"\n").unwrap();
            std::fs::set_permissions(&enter, std::fs::Permissions::from_mode(0o700)).unwrap();
            command
                .arg("--distrobox-enter")
                .arg(enter)
                .env("XDG_RUNTIME_DIR", runtime.path());
        }
        command
            .args(["--", "send-state", "--config"])
            .arg(&config)
            .arg("--working-directory")
            .arg(directory.path())
            .arg("--generation")
            .arg(&snapshot.summary.generation);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::command::run_with_stdin(command, Stdio::from(child), Duration::from_secs(60))
            });
            let (begin, fd): (serde_json::Value, _) = parent.receive().unwrap();
            assert!(fd.is_none());
            assert_eq!(begin["Begin"]["files"], 1);
            assert_eq!(begin["Begin"]["assets"], 0);
            parent.send(&"Ready", None).unwrap();
            let (state, fd): (serde_json::Value, _) = parent.receive().unwrap();
            assert_eq!(state["State"]["path"], "config.json");
            let bytes = crate::state_transfer::read_sealed_state(
                fd.unwrap(),
                state["State"]["bytes"].as_u64().unwrap() as usize,
                state["State"]["digest"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                settings
            );
            parent.send(&"StateAccepted", None).unwrap();
            let (finish, fd): (String, _) = parent.receive().unwrap();
            assert_eq!(finish, "Finish");
            assert!(fd.is_none());
            parent.send(&"Prepared", None).unwrap();
            let (accepted, fd): (String, _) = parent.receive().unwrap();
            assert_eq!(accepted, "Accepted");
            assert!(fd.is_none());
            let output = worker.join().unwrap().unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                output.stdout,
                output.stderr
            );
            let summary: crate::state::StateSummary = serde_json::from_str(&output.stdout).unwrap();
            assert_eq!(summary.generation, snapshot.summary.generation);
        });
        let mut check = Command::new(binaries.join("lianli-control"));
        check
            .args(["box-worker", "--box", "lianli-release-box", "--binaries"])
            .arg(&binaries);
        if local_adapter {
            check
                .arg("--distrobox-enter")
                .arg(runtime.path().join("distrobox-enter"))
                .env("XDG_RUNTIME_DIR", runtime.path());
        }
        check
            .args(["--", "check-saved-state", "--config"])
            .arg(&config)
            .arg("--working-directory")
            .arg(directory.path());
        let output = crate::command::run(check, Duration::from_secs(60)).unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            output.stdout,
            output.stderr
        );
        let checked: crate::saved_state::Checked = serde_json::from_str(&output.stdout).unwrap();
        assert_eq!(
            checked.state.as_deref(),
            Some(snapshot.summary.generation.as_str())
        );
        let mut cleanup = Command::new(binaries.join("lianli-control"));
        cleanup
            .args(["box-worker", "--box", "lianli-release-box", "--binaries"])
            .arg(&binaries);
        if local_adapter {
            cleanup
                .arg("--distrobox-enter")
                .arg(runtime.path().join("distrobox-enter"))
                .env("XDG_RUNTIME_DIR", runtime.path());
        }
        cleanup.args(["--", "finish-transfer"]);
        let mut input = crate::state_transfer::sealed_state(b"{}").unwrap();
        input.rewind().unwrap();
        let output =
            crate::command::run_with_stdin(cleanup, Stdio::from(input), Duration::from_secs(60))
                .unwrap();
        assert!(!output.status.success());
        assert!(output.stderr.contains("missing field"), "{}", output.stderr);
    }
}
