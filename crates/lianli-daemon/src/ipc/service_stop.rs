use lianli_shared::daemon::{parse_service_invocation, DaemonInfo, DaemonMode};
use lianli_shared::ipc::IpcResponse;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub(super) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of_val(&credentials) {
        return Err(io::Error::other("Invalid IPC peer credentials"));
    }
    Ok(credentials.uid)
}

pub(super) fn request(
    info: &DaemonInfo,
    invocation: &str,
    peer_uid: u32,
    daemon_uid: u32,
    stop: impl FnOnce() -> io::Result<()>,
) -> IpcResponse {
    if info.mode != DaemonMode::User || peer_uid != daemon_uid {
        return IpcResponse::error(
            "Only the daemon's own account may stop its user service over IPC",
        );
    }
    let invocation = match parse_service_invocation(invocation) {
        Ok(value) => value,
        Err(error) => return IpcResponse::error(error),
    };
    if info.service_invocation.as_deref() != Some(&invocation) {
        return IpcResponse::error(
            "The daemon does not belong to this service invocation; it was not stopped",
        );
    }
    match stop() {
        Ok(()) => IpcResponse::ok(serde_json::json!({"accepted": true})),
        Err(error) => IpcResponse::error(format!("Cannot request graceful shutdown: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVOCATION: &str = "abcdef0123456789abcdef0123456789";

    fn info() -> DaemonInfo {
        DaemonInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: lianli_shared::daemon::IPC_PROTOCOL_VERSION,
            instance_id: "test".into(),
            pid: 123,
            mode: DaemonMode::User,
            config_path: "/test/config.json".into(),
            capabilities: vec![],
            ownership_lock: None,
            service_invocation: Some(INVOCATION.into()),
            service_operation_lock: None,
        }
    }

    #[test]
    fn only_the_matching_user_invocation_can_request_shutdown() {
        let mut daemon = info();
        for (invocation, uid) in [
            (INVOCATION, 1001),
            ("00000000000000000000000000000001", 1000),
            ("", 1000),
        ] {
            assert!(matches!(
                request(&daemon, invocation, uid, 1000, || panic!(
                    "unauthorized stop"
                )),
                IpcResponse::Error { .. }
            ));
        }
        daemon.mode = DaemonMode::System;
        assert!(matches!(
            request(&daemon, INVOCATION, 1000, 1000, || panic!("system stop")),
            IpcResponse::Error { .. }
        ));
        daemon.mode = DaemonMode::User;
        daemon.service_invocation = None;
        assert!(matches!(
            request(&daemon, INVOCATION, 1000, 1000, || panic!("manual stop")),
            IpcResponse::Error { .. }
        ));
        daemon.service_invocation = Some(INVOCATION.into());
        let mut called = false;
        let response = request(&daemon, INVOCATION, 1000, 1000, || {
            called = true;
            Ok(())
        });
        assert!(called);
        assert!(
            matches!(response, IpcResponse::Ok { data } if data == serde_json::json!({"accepted": true}))
        );
        assert!(matches!(
            request(&daemon, INVOCATION, 1000, 1000, || Err(io::Error::other(
                "fixture failure"
            ))),
            IpcResponse::Error { .. }
        ));
    }

    #[test]
    fn socket_credentials_identify_the_actual_peer_account() {
        let (client, _peer) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&client).unwrap(), unsafe { libc::geteuid() });
    }
}
