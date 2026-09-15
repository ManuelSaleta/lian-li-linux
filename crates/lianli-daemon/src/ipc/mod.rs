//! IPC layer: Unix domain socket server + per-concern request handlers.
//!
//! The server ([`server`]) accepts connections, deserializes [`IpcRequest`]s,
//! and dispatches to one of the handler submodules:
//!
//! - [`system`] — read-only queries: ping, sensor/device enumeration, telemetry.
//! - [`config`] — config-writing handlers (`SetConfig`, `SetLcdMedia`, etc.).
//! - [`fan`] — fan-specific handlers (`SetEne6k77FanQuantity`, fan direction).
//! - [`rgb`] — RGB effect / direct-color / zone queries.
//! - [`lcd`] — display-mode switching, template preview rendering.
//! - [`wireless`] — bind / unbind RF devices.
//! - [`templates`] — LCD template CRUD.
//! - [`presets`] — RGB preset save / load / delete / apply.

mod backups;
mod catalog_cleanup;
mod event_sender;
mod installation;
mod server;
mod service_stop;

pub mod catalog;
pub mod config;
pub mod fan;
pub mod lcd;
pub mod presets;
pub mod profiles;
pub mod rgb;
pub mod system;
pub mod templates;
pub mod wireless;

pub(crate) use event_sender::EventSender;
pub use server::{build_info, start_ipc_server, DaemonState, PixelCleanState};

use lianli_shared::ipc::IpcResponse;
use parking_lot::Mutex;
use std::sync::Arc;

use crate::service::DaemonEvent;

pub(crate) use crate::persistence::{write_config, write_rgb_presets};

/// Type alias for the shared state reference handlers receive.
pub(crate) type SharedState = Arc<Mutex<DaemonState>>;

pub(crate) fn persist_and_notify(
    state: &mut DaemonState,
    tx: &EventSender,
    label: &str,
    config: lianli_shared::config::AppConfig,
) -> IpcResponse {
    use tracing::info;
    match write_config(&state.config_path, &config) {
        Ok(()) => {
            state.config = Some(config);
            let _ = tx.send(DaemonEvent::IpcUpdate);
            info!("{label}: config persisted, notified daemon");
            IpcResponse::ok(serde_json::json!(null))
        }
        Err(e) => IpcResponse::error(format!("failed to write config: {e}")),
    }
}
