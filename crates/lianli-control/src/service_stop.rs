use crate::daemon_probe;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::{
    parse_service_invocation, DaemonInfo, DaemonMode, WriteGuard, GRACEFUL_SHUTDOWN, SERVICE_STOP,
};
use lianli_shared::installation::InstallationContext;
use lianli_shared::ipc::IpcRequest;
use lianli_shared::services::ServiceScope;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub fn stop(context: &InstallationContext, invocation: &str) -> Result<()> {
    ensure!(
        !matches!(context, InstallationContext::UnsupportedContainer),
        "Unsupported container service context"
    );
    let invocation = parse_service_invocation(invocation).map_err(anyhow::Error::msg)?;
    let path = daemon_probe::socket_path(context, ServiceScope::User)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut stream, peer) = daemon_probe::connect(&path, deadline)?;
    ensure!(
        peer.uid == unsafe { libc::geteuid() } && peer.pid > 0,
        "User service IPC belongs to another account or invisible process"
    );
    let process = ProcessExit::open(peer.pid)?;
    let info: DaemonInfo = serde_json::from_value(daemon_probe::send_request(
        &mut stream,
        &IpcRequest::GetDaemonInfo,
        deadline,
        64 * 1024,
    )?)?;
    let guard = validate_destination(&info, peer.pid, &invocation)?;
    let request = IpcRequest::Guarded {
        guard,
        request: Box::new(IpcRequest::StopService {
            invocation_id: invocation,
        }),
    };
    let response = daemon_probe::send_request(&mut stream, &request, deadline, 64 * 1024)
        .context("Service stop acknowledgement was not received. The request was not retried. Recheck the service before taking another action")?;
    ensure!(
        response
            .get("accepted")
            .and_then(serde_json::Value::as_bool)
            == Some(true),
        "Service stop was not acknowledged"
    );
    process.wait(Instant::now() + Duration::from_secs(90))
        .context("The daemon has not exited. Graceful shutdown may still be running. No forced signal was sent")
}

fn validate_destination(info: &DaemonInfo, peer_pid: i32, invocation: &str) -> Result<WriteGuard> {
    ensure!(
        peer_pid > 0 && info.pid == peer_pid as u32,
        "Service IPC identity does not match its process"
    );
    ensure!(info.mode == DaemonMode::User && info.service_invocation.as_deref() == Some(invocation),
        "The running daemon does not belong to the requested user service invocation. It was not stopped");
    ensure!(
        [GRACEFUL_SHUTDOWN, SERVICE_STOP]
            .iter()
            .all(|capability| info.capabilities.iter().any(|value| value == capability)),
        "Update the daemon before using guarded service shutdown"
    );
    info.write_guard(env!("CARGO_PKG_VERSION"))
        .map_err(anyhow::Error::msg)
}

struct ProcessExit(OwnedFd);

impl ProcessExit {
    fn open(pid: i32) -> Result<Self> {
        ensure!(pid > 0, "Invalid service process ID");
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(
            descriptor >= 0,
            "Cannot monitor service exit with pidfd_open: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(unsafe { OwnedFd::from_raw_fd(descriptor as i32) }))
    }

    fn wait(&self, deadline: Instant) -> Result<()> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "Service exit verification timed out");
            let mut poll = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if result > 0 {
                ensure!(
                    poll.revents & (libc::POLLIN | libc::POLLHUP) != 0,
                    "Cannot observe service process exit"
                );
                return Ok(());
            }
            if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_refuses_another_invocation_process_scope_or_older_daemon() {
        let invocation = "abcdef0123456789abcdef0123456789";
        let mut info = DaemonInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: lianli_shared::daemon::IPC_PROTOCOL_VERSION,
            instance_id: "test".into(),
            pid: 123,
            mode: DaemonMode::User,
            config_path: "/test/config.json".into(),
            ownership_lock: None,
            service_invocation: Some(invocation.into()),
            service_operation_lock: None,
            capabilities: vec![
                lianli_shared::daemon::GUARDED_WRITES.into(),
                GRACEFUL_SHUTDOWN.into(),
                SERVICE_STOP.into(),
            ],
        };
        let guard = validate_destination(&info, 123, invocation).unwrap();
        assert_eq!(guard.instance_id, "test");
        assert!(validate_destination(&info, 456, invocation).is_err());
        assert!(validate_destination(&info, 123, "other").is_err());
        info.mode = DaemonMode::System;
        assert!(validate_destination(&info, 123, invocation).is_err());
        info.mode = DaemonMode::User;
        info.service_invocation = None;
        assert!(validate_destination(&info, 123, invocation).is_err());
        info.service_invocation = Some(invocation.into());
        info.capabilities
            .retain(|capability| capability != GRACEFUL_SHUTDOWN);
        assert!(validate_destination(&info, 123, invocation).is_err());
    }

    #[test]
    fn process_exit_wait_is_bounded_and_observes_only_its_process() {
        let own = ProcessExit::open(std::process::id() as i32).unwrap();
        assert!(own.wait(Instant::now() + Duration::from_millis(5)).is_err());
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let process = ProcessExit::open(child.id() as i32).unwrap();
        let result = process.wait(Instant::now() + Duration::from_secs(5));
        child.wait().unwrap();
        result.unwrap();
        assert!(own.wait(Instant::now() + Duration::from_millis(5)).is_err());
    }
}
