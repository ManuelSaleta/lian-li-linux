//! Fan-specific IPC handlers: `SetEne6k77FanQuantity`.

use super::EventSender;

use lianli_shared::ipc::IpcResponse;

use crate::service::DaemonEvent;

pub fn validate_quantities(
    config: &lianli_shared::config::AppConfig,
    devices: &[lianli_shared::ipc::DeviceInfo],
) -> Option<String> {
    for (serial, controller) in &config.ene6k77 {
        let max = devices
            .iter()
            .find(|device| {
                (device.serial.as_ref() == Some(serial)
                    || device
                        .device_id
                        .rsplit_once(":port")
                        .is_some_and(|(base, _)| base == serial))
                    && device.max_fan_quantity.is_some()
            })
            .and_then(|device| device.max_fan_quantity)
            .unwrap_or(6);
        if controller
            .fan_quantities
            .iter()
            .any(|(&port, &quantity)| port >= 4 || quantity > max)
        {
            return Some(format!(
                "Invalid ENE fan quantities for {serial}: ports must be 0–3 and quantities 0–{max}"
            ));
        }
    }
    None
}

pub fn set_ene6k77_fan_quantity(tx: EventSender, device_id: String, quantity: u8) -> IpcResponse {
    let (reply, received) = std::sync::mpsc::sync_channel(1);
    if tx
        .send(DaemonEvent::SetEne6k77FanQuantity {
            device_id,
            quantity,
            reply,
        })
        .is_err()
    {
        return IpcResponse::error("Daemon service is not running");
    }
    match received.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(())) => IpcResponse::ok(serde_json::json!({"message": "Fan quantity saved and applied."})),
        Ok(Err(error)) => IpcResponse::error(error),
        Err(_) => IpcResponse::error("Fan quantity update is unconfirmed and may still complete. Refresh device state before retrying."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::{config::AppConfig, ipc::DeviceInfo};
    use std::sync::mpsc;

    #[test]
    fn quantity_response_reports_service_result_and_disconnection() {
        for result in [Ok(()), Err("USB write failed".to_string())] {
            let (tx, rx) = mpsc::channel();
            let expected_ok = result.is_ok();
            let worker = std::thread::spawn(move || {
                let DaemonEvent::SetEne6k77FanQuantity {
                    device_id,
                    quantity,
                    reply,
                } = rx.recv().unwrap()
                else {
                    panic!("wrong event")
                };
                assert_eq!(device_id, "controller:port2");
                assert_eq!(quantity, 3);
                reply.send(result).unwrap();
            });
            let response = set_ene6k77_fan_quantity(tx.into(), "controller:port2".into(), 3);
            assert_eq!(matches!(response, IpcResponse::Ok { .. }), expected_ok);
            worker.join().unwrap();
        }
        let (tx, rx) = mpsc::channel();
        drop(rx);
        assert!(matches!(
            set_ene6k77_fan_quantity(tx.into(), "controller:port2".into(), 3),
            IpcResponse::Error { .. }
        ));
    }

    #[test]
    fn quantity_validation_uses_connected_model_limits_and_preserves_offline_six_fan_groups() {
        let mut config = AppConfig::default();
        config
            .ene6k77
            .entry("serial".into())
            .or_default()
            .fan_quantities
            .insert(2, 6);
        let mut device: DeviceInfo = serde_json::from_value(serde_json::json!({
            "device_id": "controller:port2", "serial": "serial", "family": "Ene6k77", "name": "ENE", "has_lcd": false, "has_fan": true, "has_pump": false, "has_rgb": true, "mb_sync_support": true, "max_fan_quantity": 4
        })).unwrap();
        assert!(validate_quantities(&config, &[]).is_none());
        assert!(validate_quantities(&config, &[device.clone()]).is_some());
        device.max_fan_quantity = Some(6);
        assert!(validate_quantities(&config, &[device]).is_none());
        config
            .ene6k77
            .get_mut("serial")
            .unwrap()
            .fan_quantities
            .insert(4, 0);
        assert!(validate_quantities(&config, &[]).is_some());
    }
}
