//! Newline-delimited JSON over the selected daemon's Unix socket.

use lianli_shared::daemon::DaemonInfo;
use lianli_shared::ipc::{IpcRequest, IpcResponse, TelemetrySnapshot};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tracing::debug;

const TIMEOUT: Duration = Duration::from_secs(5);

const SYSTEM_SOCKET: &str = "/run/lianli/lianli-daemon.sock";

/// Last socket that accepted a connection
static ACTIVE_SOCKET: OnceLock<Mutex<Option<String>>> = OnceLock::new();
fn active_lock() -> &'static Mutex<Option<String>> {
    ACTIVE_SOCKET.get_or_init(|| Mutex::new(None))
}

fn user_socket() -> String {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{runtime_dir}/lianli-daemon.sock")
}

/// Candidate daemon socket paths
fn candidate_paths() -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut v: Vec<String> = Vec::new();
    for p in active_lock()
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .chain(std::iter::once(user_socket()))
        .chain(std::iter::once(SYSTEM_SOCKET.to_string()))
    {
        if seen.insert(p.clone()) {
            v.push(p);
        }
    }
    v
}

/// Socket path to surface to the UI (the active one, else the per-user default).
pub fn socket_path() -> String {
    active_lock()
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(user_socket)
}

/// Combined result of a single poll cycle, returned to the frontend store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PollResult {
    pub connected: bool,
    pub socket_path: String,
    #[serde(default)]
    pub daemon_info: Option<DaemonInfo>,
    pub write_error: Option<String>,
    pub devices: Vec<lianli_shared::ipc::DeviceInfo>,
    pub telemetry: TelemetrySnapshot,
}

fn send_raw(request: IpcRequest) -> Result<IpcResponse, String> {
    let mut last_err: Option<String> = None;

    for path in candidate_paths() {
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(e) => {
                last_err = Some(format!("cannot connect to daemon at {path}: {e}"));
                continue;
            }
        };
        let request = prepare_request(request, || {
            exchange(&stream, r#"{"method":"GetDaemonInfo"}"#, false)
        })?;
        let json = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        match ipc_round_trip(stream, &json) {
            Ok(resp) => {
                *active_lock().lock().unwrap() = Some(path);
                return Ok(resp);
            }
            Err(e) => {
                *active_lock().lock().unwrap() = None;
                return Err(format!("Daemon response failed; the request was not retried because it may have been applied: {e}"));
            }
        }
    }

    *active_lock().lock().unwrap() = None;
    Err(last_err.unwrap_or_else(|| "no daemon socket candidates".to_string()))
}

fn prepare_request(
    request: IpcRequest,
    read_info: impl FnOnce() -> Result<IpcResponse, String>,
) -> Result<IpcRequest, String> {
    if matches!(request, IpcRequest::Guarded { .. }) {
        return Err("The GUI backend supplies compatibility checks".into());
    }
    if request.is_read_only() {
        return Ok(request);
    }
    let info: DaemonInfo = serde_json::from_value(response_data(read_info()?)?)
        .map_err(|error| format!("Cannot verify daemon compatibility: {error}"))?;
    Ok(IpcRequest::Guarded {
        guard: info.write_guard(env!("CARGO_PKG_VERSION"))?,
        request: Box::new(request),
    })
}

/// Write one JSON request on a connected stream and read one JSON response.
fn ipc_round_trip(stream: UnixStream, json: &str) -> Result<IpcResponse, String> {
    exchange(&stream, json, true)
}

fn exchange(stream: &UnixStream, json: &str, finish: bool) -> Result<IpcResponse, String> {
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|error| error.to_string())?;

    {
        let mut writer = stream;
        writer
            .write_all(json.as_bytes())
            .map_err(|e| format!("write error: {e}"))?;
        writer
            .write_all(b"\n")
            .map_err(|e| format!("write error: {e}"))?;
        writer.flush().map_err(|e| format!("flush error: {e}"))?;
    }

    if finish {
        stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(|e| format!("shutdown error: {e}"))?;
    }

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|e| format!("read error: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let response: IpcResponse =
            serde_json::from_str(&line).map_err(|e| format!("parse error: {e}"))?;
        return Ok(response);
    }

    Err("no response from daemon".to_string())
}

pub fn request(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let req = serde_json::from_value(serde_json::json!({ "method": method, "params": params }))
        .map_err(|error| format!("Unsupported request: {error}"))?;
    debug!("ipc -> {method}");
    match send_raw(req)? {
        IpcResponse::Ok { data } => Ok(data),
        IpcResponse::Error { message } => Err(message),
    }
}

/// Quick liveness check — a single `Ping`.
pub fn ping() -> bool {
    match request("Ping", serde_json::Value::Null) {
        Ok(_) => true,
        Err(e) => {
            debug!("ping failed: {e}");
            false
        }
    }
}

pub fn poll() -> PollResult {
    for path in candidate_paths() {
        let result = poll_at(&path, |method| {
            let stream = UnixStream::connect(&path).map_err(|e| e.to_string())?;
            let json = serde_json::json!({ "method": method, "params": null }).to_string();
            ipc_round_trip(stream, &json)
        });
        match result {
            Ok(result) => {
                *active_lock().lock().unwrap() = Some(path);
                return result;
            }
            Err(error) => debug!("poll at {path} failed: {error}"),
        }
    }
    *active_lock().lock().unwrap() = None;
    PollResult {
        socket_path: user_socket(),
        ..Default::default()
    }
}

fn poll_at(
    path: &str,
    mut send: impl FnMut(&str) -> Result<IpcResponse, String>,
) -> Result<PollResult, String> {
    let daemon_info: Option<DaemonInfo> = match send("GetDaemonInfo")? {
        IpcResponse::Ok { data } => Some(
            serde_json::from_value(data).map_err(|e| format!("invalid daemon identity: {e}"))?,
        ),
        IpcResponse::Error { message } if message.contains("unknown variant `GetDaemonInfo`") => {
            response_data(send("Ping")?)?;
            None
        }
        IpcResponse::Error { message } => return Err(message),
    };
    let devices = serde_json::from_value(response_data(send("ListDevices")?)?)
        .map_err(|e| format!("invalid device list: {e}"))?;
    let telemetry = serde_json::from_value(response_data(send("GetTelemetry")?)?)
        .map_err(|e| format!("invalid telemetry: {e}"))?;
    let write_error = match &daemon_info {
        Some(info) => info.write_guard(env!("CARGO_PKG_VERSION")).err(),
        None => Some("Changes are disabled: this daemon cannot report compatibility. Update both applications and restart the selected daemon cleanly.".into()),
    };

    Ok(PollResult {
        connected: true,
        socket_path: path.into(),
        daemon_info,
        write_error,
        devices,
        telemetry,
    })
}

fn response_data(response: IpcResponse) -> Result<serde_json::Value, String> {
    match response {
        IpcResponse::Ok { data } => Ok(data),
        IpcResponse::Error { message } => Err(message),
    }
}

pub fn connection_info() -> (bool, String) {
    (ping(), socket_path().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::daemon::{DaemonMode, IPC_PROTOCOL_VERSION};

    #[test]
    fn identity_and_write_share_one_connection_without_closing_it_early() {
        let (client, server) = UnixStream::pair().unwrap();
        server.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut daemon = info("same-peer");
        daemon.version = env!("CARGO_PKG_VERSION").into();
        daemon
            .capabilities
            .push(lianli_shared::daemon::GUARDED_WRITES.into());
        let peer = std::thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str::<IpcRequest>(&line).unwrap(),
                IpcRequest::GetDaemonInfo
            ));
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&IpcResponse::ok(&daemon)).unwrap()
            )
            .unwrap();
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            let request = serde_json::from_str::<IpcRequest>(&line).unwrap();
            assert!(matches!(
                request.authorize(&daemon).unwrap(),
                IpcRequest::SetLcdTemplates { .. }
            ));
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&IpcResponse::ok("applied")).unwrap()
            )
            .unwrap();
            line.clear();
            assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        });
        let request = prepare_request(IpcRequest::SetLcdTemplates { templates: vec![] }, || {
            exchange(&client, r#"{"method":"GetDaemonInfo"}"#, false)
        })
        .unwrap();
        let response = ipc_round_trip(client, &serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(response_data(response).unwrap(), "applied");
        peer.join().unwrap();
    }

    fn info(instance: &str) -> DaemonInfo {
        DaemonInfo {
            version: "1.0.0".into(),
            protocol_version: IPC_PROTOCOL_VERSION,
            instance_id: instance.into(),
            pid: 123,
            mode: DaemonMode::System,
            config_path: "/var/lib/lianli/config.json".into(),
            ownership_lock: None,
            service_invocation: None,
            service_operation_lock: None,
            capabilities: vec!["daemon_info".into()],
        }
    }

    #[test]
    fn reads_do_not_negotiate_but_writes_require_current_identity() {
        assert!(prepare_request(IpcRequest::GetConfig, || panic!("read negotiated")).is_ok());
        let write = || IpcRequest::SetLcdTemplates { templates: vec![] };
        assert!(prepare_request(write(), || Ok(IpcResponse::error("unknown method"))).is_err());
        assert!(prepare_request(write(), || Ok(IpcResponse::ok(info("old")))).is_err());
        let mut current = info("current");
        current.version = env!("CARGO_PKG_VERSION").into();
        current
            .capabilities
            .push(lianli_shared::daemon::GUARDED_WRITES.into());
        let request = prepare_request(write(), || Ok(IpcResponse::ok(&current))).unwrap();
        assert!(matches!(
            request.authorize(&current).unwrap(),
            IpcRequest::SetLcdTemplates { .. }
        ));
    }

    #[test]
    fn poll_reports_the_current_instance_without_an_extra_ping() {
        for instance in ["first", "restarted"] {
            let result = poll_at("/test.sock", |method| {
                Ok(match method {
                    "GetDaemonInfo" => IpcResponse::ok(info(instance)),
                    "ListDevices" => IpcResponse::ok(serde_json::json!([])),
                    "GetTelemetry" => IpcResponse::ok(TelemetrySnapshot::default()),
                    other => panic!("unexpected {other}"),
                })
            })
            .unwrap();
            assert!(result.connected);
            assert_eq!(result.daemon_info.unwrap().instance_id, instance);
        }
    }

    #[test]
    fn legacy_daemon_still_connects_without_fabricated_identity() {
        let result = poll_at("/legacy.sock", |method| {
            Ok(match method {
                "GetDaemonInfo" => {
                    IpcResponse::error("invalid request: unknown variant `GetDaemonInfo`")
                }
                "Ping" => IpcResponse::ok("pong"),
                "ListDevices" => IpcResponse::ok(serde_json::json!([])),
                "GetTelemetry" => IpcResponse::ok(TelemetrySnapshot::default()),
                other => panic!("unexpected {other}"),
            })
        })
        .unwrap();
        assert!(result.connected);
        assert!(result.daemon_info.is_none());
        assert!(result.write_error.is_some());
    }

    #[test]
    fn failed_telemetry_is_not_reported_as_a_healthy_empty_snapshot() {
        let error = poll_at("/test.sock", |method| {
            Ok(match method {
                "GetDaemonInfo" => IpcResponse::ok(info("first")),
                "ListDevices" => IpcResponse::ok(serde_json::json!([])),
                "GetTelemetry" => IpcResponse::error("service unavailable"),
                other => panic!("unexpected {other}"),
            })
        })
        .unwrap_err();
        assert_eq!(error, "service unavailable");
    }
}
