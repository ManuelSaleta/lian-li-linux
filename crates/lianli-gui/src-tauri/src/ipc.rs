//! Unix-socket bridge to the lianli-daemon.
//!
//! Mirrors the protocol used by the Slint GUI's `ipc_client.rs`: newline-
//! delimited JSON over `$XDG_RUNTIME_DIR/lianli-daemon.sock`. Each request
//! opens a fresh connection, writes one JSON line, shuts down the write half,
//! and reads exactly one response line.

use lianli_shared::ipc::{IpcResponse, TelemetrySnapshot};
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
    pub devices: Vec<lianli_shared::ipc::DeviceInfo>,
    pub telemetry: TelemetrySnapshot,
}

/// Send a raw JSON request object to the daemon and return the parsed response.
fn send_raw(request: &serde_json::Value) -> Result<IpcResponse, String> {
    let json = serde_json::to_string(request).map_err(|e| format!("serialize error: {e}"))?;
    let mut last_err: Option<String> = None;

    for path in candidate_paths() {
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(e) => {
                last_err = Some(format!("cannot connect to daemon at {path}: {e}"));
                continue;
            }
        };
        match ipc_round_trip(stream, &json) {
            Ok(resp) => {
                *active_lock().lock().unwrap() = Some(path);
                return Ok(resp);
            }
            Err(e) => {
                *active_lock().lock().unwrap() = None;
                last_err = Some(e);
            }
        }
    }

    *active_lock().lock().unwrap() = None;
    Err(last_err.unwrap_or_else(|| "no daemon socket candidates".to_string()))
}

/// Write one JSON request on a connected stream and read one JSON response.
fn ipc_round_trip(stream: UnixStream, json: &str) -> Result<IpcResponse, String> {
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    {
        let mut writer = &stream;
        writer
            .write_all(json.as_bytes())
            .map_err(|e| format!("write error: {e}"))?;
        writer
            .write_all(b"\n")
            .map_err(|e| format!("write error: {e}"))?;
        writer.flush().map_err(|e| format!("flush error: {e}"))?;
    }

    // Shut down the write side so the daemon sees EOF while reading.
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("shutdown error: {e}"))?;

    let reader = BufReader::new(&stream);
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

/// Issue any IPC method by name, forwarding arbitrary params.
///
/// The request is serialized as `{"method": <method>, "params": <params>}`,
/// matching the daemon's `#[serde(tag = "method", content = "params")]` wire
/// format. On an `Ok` response the inner `data` value is returned; on `Error`
/// the message is propagated as `Err`.
pub fn request(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let req = serde_json::json!({ "method": method, "params": params });
    debug!("ipc -> {method}");
    match send_raw(&req)? {
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

/// Issue a Ping + ListDevices + GetTelemetry in sequence and bundle the result.
pub fn poll() -> PollResult {
    let connected = ping();
    let path = socket_path().to_string();
    if !connected {
        return PollResult {
            connected: false,
            socket_path: path,
            ..Default::default()
        };
    }

    let devices: Vec<lianli_shared::ipc::DeviceInfo> =
        serde_json::from_value(request("ListDevices", serde_json::Value::Null).unwrap_or_default())
            .unwrap_or_default();
    let telemetry: TelemetrySnapshot = serde_json::from_value(
        request("GetTelemetry", serde_json::Value::Null).unwrap_or_default(),
    )
    .unwrap_or_default();

    PollResult {
        connected: true,
        socket_path: path,
        devices,
        telemetry,
    }
}

/// Fetch the daemon's reported version string (best-effort, parsed from a
/// `Ping`-style probe). The daemon does not currently expose a dedicated
/// version IPC, so we surface the socket path and connection state instead.
pub fn connection_info() -> (bool, String) {
    (ping(), socket_path().to_string())
}
