use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::{DaemonInfo, DaemonMode};
use lianli_shared::installation::InstallationContext;
use lianli_shared::ipc::{IpcRequest, IpcResponse};
use lianli_shared::services::{OwnershipSnapshot, ServiceProbe, ServiceScope};
use socket2::{Domain, SockAddr, Socket, Type};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn inspect(
    context: &InstallationContext,
    scope: ServiceScope,
    owner: &OwnershipSnapshot,
) -> Result<DaemonInfo> {
    let process = match &owner.process {
        Some(ServiceProbe::Known { value }) => value,
        _ => anyhow::bail!("Hardware owner process is unverified"),
    };
    let path = socket_path(context, scope)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut stream, peer) = connect(&path, deadline)?;
    ensure!(
        peer.uid == process.effective_uid,
        "Service IPC belongs to another account"
    );
    if matches!(context, InstallationContext::Native) {
        ensure!(
            peer.pid > 0 && peer.pid as u32 == process.pid,
            "Service IPC belongs to another process"
        );
    } else {
        crate::process_owner::verify_container_peer(
            &crate::services::Route::detect(context)?,
            process,
            peer.pid,
        )?;
    }
    let info: DaemonInfo =
        serde_json::from_value(request(&mut stream, "GetDaemonInfo", deadline, 64 * 1024)?)?;
    validate_info(&info, owner, scope)?;
    let _: lianli_shared::config::AppConfig = serde_json::from_value(request(
        &mut stream,
        "GetConfig",
        deadline,
        16 * 1024 * 1024,
    )?)
    .context("The service did not return its loaded configuration")?;
    Ok(info)
}

pub(crate) fn socket_path(context: &InstallationContext, scope: ServiceScope) -> Result<PathBuf> {
    Ok(match scope {
        ServiceScope::System => PathBuf::from("/run/lianli/lianli-daemon.sock"),
        ServiceScope::User => {
            let runtime = if matches!(context, InstallationContext::Distrobox { .. }) {
                PathBuf::from(
                    std::env::var_os("XDG_RUNTIME_DIR")
                        .context("Missing container runtime directory")?,
                )
            } else {
                PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() }))
            };
            ensure!(
                runtime.is_absolute(),
                "User runtime directory must be absolute"
            );
            runtime.join("lianli-daemon.sock")
        }
    })
}

pub(crate) fn connect(
    path: &std::path::Path,
    deadline: Instant,
) -> Result<(UnixStream, libc::ucred)> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    if let Err(error) = socket.connect(&SockAddr::unix(path)?) {
        ensure!(
            error.raw_os_error() == Some(libc::EINPROGRESS),
            "Connecting service IPC: {error}"
        );
        wait(socket.as_raw_fd(), libc::POLLOUT, deadline)?;
        if let Some(error) = socket.take_error()? {
            return Err(error.into());
        }
    }
    let descriptor: OwnedFd = socket.into();
    let stream = UnixStream::from(descriptor);
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&peer) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut length,
            )
        } == 0
            && length as usize == std::mem::size_of_val(&peer),
        "Cannot verify service IPC peer"
    );
    Ok((stream, peer))
}

fn validate_info(info: &DaemonInfo, owner: &OwnershipSnapshot, scope: ServiceScope) -> Result<()> {
    info.write_guard(env!("CARGO_PKG_VERSION"))
        .map_err(anyhow::Error::msg)?;
    ensure!(info.capabilities.iter().any(|value| value == lianli_shared::daemon::GRACEFUL_SHUTDOWN),
        "This daemon still lacks verified graceful shutdown support. Update the daemon binary and restart it cleanly before using service controls");
    ensure!(
        info.ownership_lock.as_ref() == Some(&owner.identity),
        "Service IPC reports another hardware lock"
    );
    ensure!(
        info.mode
            == match scope {
                ServiceScope::User => DaemonMode::User,
                ServiceScope::System => DaemonMode::System,
            },
        "Service started with a different configuration mode"
    );
    Ok(())
}

fn wait(fd: i32, events: i16, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Service IPC verification timed out");
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if result > 0 {
            return Ok(());
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
    }
}

fn request(
    stream: &mut UnixStream,
    method: &str,
    deadline: Instant,
    limit: usize,
) -> Result<serde_json::Value> {
    let command = format!("{{\"method\":\"{method}\"}}\n");
    exchange(stream, &command, deadline, limit)
}

pub(crate) fn send_request(
    stream: &mut UnixStream,
    request: &IpcRequest,
    deadline: Instant,
    limit: usize,
) -> Result<serde_json::Value> {
    let mut command = serde_json::to_string(request)?;
    command.push('\n');
    exchange(stream, &command, deadline, limit)
}

fn exchange(
    stream: &mut UnixStream,
    command: &str,
    deadline: Instant,
    limit: usize,
) -> Result<serde_json::Value> {
    let mut remaining = command.as_bytes();
    while !remaining.is_empty() {
        ensure!(
            Instant::now() < deadline,
            "Service IPC verification timed out"
        );
        match stream.write(remaining) {
            Ok(0) => anyhow::bail!("Service IPC disconnected"),
            Ok(count) => remaining = &remaining[count..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait(stream.as_raw_fd(), libc::POLLOUT, deadline)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut response = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        ensure!(
            Instant::now() < deadline,
            "Service IPC verification timed out"
        );
        match stream.read(&mut buffer) {
            Ok(0) => anyhow::bail!("Service IPC disconnected before replying"),
            Ok(count) => {
                ensure!(
                    response.len() + count <= limit,
                    "Service IPC response exceeds its size limit"
                );
                response.extend_from_slice(&buffer[..count]);
                if buffer[..count].contains(&b'\n') {
                    return match serde_json::from_slice::<IpcResponse>(&response)? {
                        IpcResponse::Ok { data } => Ok(data),
                        IpcResponse::Error { message } => anyhow::bail!("Service IPC: {message}"),
                    };
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait(stream.as_raw_fd(), libc::POLLIN, deadline)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_stop_transmits_the_guard_and_invocation_in_one_request() {
        use std::io::BufRead;
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        let request = IpcRequest::Guarded {
            guard: lianli_shared::daemon::WriteGuard {
                client_version: "1.0.0".into(),
                protocol_version: 1,
                instance_id: "instance".into(),
            },
            request: Box::new(IpcRequest::StopService {
                invocation_id: "abcdef0123456789abcdef0123456789".into(),
            }),
        };
        let expected = serde_json::to_value(&request).unwrap();
        let worker = std::thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&server)
                .read_line(&mut line)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap(),
                expected
            );
            server
                .write_all(b"{\"status\":\"ok\",\"data\":{\"accepted\":true}}\n")
                .unwrap();
        });
        assert_eq!(
            send_request(
                &mut client,
                &request,
                Instant::now() + Duration::from_secs(1),
                1024
            )
            .unwrap(),
            serde_json::json!({"accepted": true})
        );
        worker.join().unwrap();
    }

    #[test]
    fn service_controls_reject_old_forced_exit_daemons_even_at_the_same_version() {
        let owner = OwnershipSnapshot {
            identity: lianli_shared::daemon::FileIdentity {
                device: "1".into(),
                inode: "2".into(),
            },
            owner_pid: Some(123),
            process: None,
        };
        let mut info = DaemonInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: lianli_shared::daemon::IPC_PROTOCOL_VERSION,
            instance_id: "instance".into(),
            pid: 123,
            mode: DaemonMode::User,
            config_path: "/config.json".into(),
            capabilities: vec![lianli_shared::daemon::GUARDED_WRITES.into()],
            ownership_lock: Some(owner.identity.clone()),
            service_invocation: None,
            service_operation_lock: None,
        };
        assert!(validate_info(&info, &owner, ServiceScope::User).is_err());
        info.capabilities
            .push(lianli_shared::daemon::GRACEFUL_SHUTDOWN.into());
        assert!(validate_info(&info, &owner, ServiceScope::User).is_ok());
        assert!(validate_info(&info, &owner, ServiceScope::System).is_err());
        info.ownership_lock = None;
        assert!(validate_info(&info, &owner, ServiceScope::User).is_err());
    }

    #[test]
    fn replies_must_be_bounded_complete_json_and_respect_the_deadline() {
        let oversized = vec![b'x'; 65];
        for (reply, expected) in [
            (b"{\"status\":\"ok\",\"data\":42}\n".as_slice(), true),
            (b"garbage\n".as_slice(), false),
            (oversized.as_slice(), false),
        ] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client.set_nonblocking(true).unwrap();
            let bytes = reply.to_vec();
            let thread = std::thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let expected = b"{\"method\":\"GetDaemonInfo\"}\n";
                let mut input = vec![0; expected.len()];
                server.read_exact(&mut input).unwrap();
                assert_eq!(input, expected);
                server.write_all(&bytes).unwrap();
            });
            assert_eq!(
                request(
                    &mut client,
                    "GetDaemonInfo",
                    Instant::now() + Duration::from_secs(1),
                    64
                )
                .is_ok(),
                expected
            );
            thread.join().unwrap();
        }
        let (mut client, _server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        assert!(request(
            &mut client,
            "GetDaemonInfo",
            Instant::now() + Duration::from_millis(20),
            64
        )
        .is_err());
    }
}
