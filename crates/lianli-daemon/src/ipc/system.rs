//! System-level queries: `Ping`, `ListSensors`, `ListDevices`, `GetConfig`,
//! `GetTelemetry`.

use lianli_shared::ipc::IpcResponse;

pub fn retry_openrgb(state: &super::SharedState, tx: super::EventSender) -> IpcResponse {
    let mut state = state.lock();
    let status = &state.telemetry.openrgb_status;
    if !status.enabled || status.running || status.error.is_none() {
        return IpcResponse::error("OpenRGB is not in a failed enabled state; refresh its status");
    }
    if state.openrgb_retry_pending {
        return IpcResponse::error("An OpenRGB retry is already queued");
    }
    state.openrgb_retry_pending = true;
    if tx.send(crate::service::DaemonEvent::RetryOpenRgb).is_err() {
        state.openrgb_retry_pending = false;
        return IpcResponse::error("The daemon stopped before accepting the OpenRGB retry");
    }
    IpcResponse::ok(serde_json::json!(null))
}

use crate::ipc::SharedState;

pub fn ping() -> IpcResponse {
    IpcResponse::ok(serde_json::json!("pong"))
}

pub fn daemon_info(state: &SharedState) -> IpcResponse {
    let (mut info, gate) = {
        let state = state.lock();
        (state.info.clone(), state.write_gate.clone())
    };
    info.service_operation_lock = gate.identity().ok();
    IpcResponse::ok(info)
}

pub fn list_sensors(state: &SharedState) -> IpcResponse {
    let mut sensors = lianli_shared::sensors::enumerate_sensors();
    // Add wireless coolant sensors from live telemetry
    let ipc_state = state.lock();
    for (device_id, temp) in &ipc_state.telemetry.coolant_temps {
        let display = ipc_state
            .devices
            .iter()
            .find(|d| d.device_id == *device_id)
            .map(|d| format!("{} (Coolant)", d.name))
            .unwrap_or_else(|| format!("{device_id} (Coolant)"));
        sensors.push(lianli_shared::sensors::SensorInfo {
            source: lianli_shared::sensors::SensorSource::WirelessCoolant {
                device_id: device_id.clone(),
            },
            sensor_name: None,
            display_name: Some(display),
            divider: 1,
            unit: lianli_shared::sensors::Unit::C,
            current_value: Some(*temp),
        });
    }
    IpcResponse::ok(&sensors)
}

pub fn list_pwm_headers() -> IpcResponse {
    let headers = lianli_shared::sensors::enumerate_pwm_headers();
    let result: Vec<serde_json::Value> = headers
        .iter()
        .map(|h| {
            let pct = lianli_shared::sensors::read_pwm_header(&h.id)
                .map(|v| (v as f32 / 255.0 * 100.0).round() as u8)
                .unwrap_or(0);
            serde_json::json!({
                "id": h.id,
                "label": format!("{} ({}%)", h.label, pct),
            })
        })
        .collect();
    IpcResponse::ok(&result)
}

pub fn list_devices(state: &SharedState) -> IpcResponse {
    let ipc_state = state.lock();
    IpcResponse::ok(&ipc_state.devices)
}

pub fn get_config(state: &SharedState) -> IpcResponse {
    let ipc_state = state.lock();
    IpcResponse::ok(&ipc_state.config)
}

pub fn get_telemetry(state: &SharedState) -> IpcResponse {
    let ipc_state = state.lock();
    IpcResponse::ok(&ipc_state.telemetry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::DaemonEvent;
    use parking_lot::Mutex;
    use std::sync::{mpsc, Arc};

    #[test]
    fn openrgb_retry_requires_failure_coalesces_requests_and_reports_queue_failure() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(crate::ipc::DaemonState::new(
            root.path().join("config.json"),
        )));
        let (tx, rx) = mpsc::channel();
        assert!(matches!(
            retry_openrgb(&state, tx.clone().into()),
            IpcResponse::Error { .. }
        ));
        {
            let mut state = state.lock();
            state.telemetry.openrgb_status.enabled = true;
            state.telemetry.openrgb_status.error = Some("Port in use".into());
        }
        assert!(matches!(
            retry_openrgb(&state, tx.clone().into()),
            IpcResponse::Ok { .. }
        ));
        assert!(state.lock().openrgb_retry_pending);
        assert!(matches!(
            retry_openrgb(&state, tx.clone().into()),
            IpcResponse::Error { .. }
        ));
        assert!(matches!(rx.try_recv().unwrap(), DaemonEvent::RetryOpenRgb));
        assert!(rx.try_recv().is_err());
        assert!(state.lock().config.is_none());
        state.lock().openrgb_retry_pending = false;
        drop(rx);
        assert!(matches!(
            retry_openrgb(&state, tx.into()),
            IpcResponse::Error { .. }
        ));
        assert!(!state.lock().openrgb_retry_pending);
    }
}
