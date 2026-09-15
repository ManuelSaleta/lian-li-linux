//! Fan-specific IPC handlers: `SetEne6k77FanQuantity`.

use super::EventSender;

use lianli_shared::ipc::IpcResponse;

use crate::service::DaemonEvent;

pub fn set_ene6k77_fan_quantity(tx: EventSender, device_id: String, quantity: u8) -> IpcResponse {
    let _ = tx.send(DaemonEvent::SetEne6k77FanQuantity {
        device_id,
        quantity,
    });
    IpcResponse::ok(serde_json::json!({
        "message": "Fan quantity update queued."
    }))
}
