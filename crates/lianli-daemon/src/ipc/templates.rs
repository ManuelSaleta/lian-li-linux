//! LCD template IPC handlers: `GetLcdTemplates`, `SetLcdTemplates`.

use super::EventSender;

use lianli_shared::ipc::IpcResponse;
use lianli_shared::template::LcdTemplate;
use tracing::info;

use crate::ipc::SharedState;
use crate::service::DaemonEvent;
use crate::template_store;

pub fn get(state: &SharedState) -> IpcResponse {
    let state = state.lock();
    let sensors = lianli_shared::sensors::enumerate_sensors();
    let all = template_store::all_templates(&state.user_templates, &sensors);
    IpcResponse::ok(&all)
}

pub fn set(state: &SharedState, tx: EventSender, templates: Vec<LcdTemplate>) -> IpcResponse {
    let mut state = state.lock();
    let path = state.templates_path();
    match template_store::save_user_templates(&path, &templates) {
        Ok(()) => {
            state.user_templates = templates;
            let _ = tx.send(DaemonEvent::IpcUpdate);
            info!("LCD templates updated via IPC");
            IpcResponse::ok(serde_json::json!(null))
        }
        Err(e) => IpcResponse::error(format!("failed to write templates: {e}")),
    }
}

pub fn merge(
    state: &SharedState,
    tx: EventSender,
    originals: Vec<LcdTemplate>,
    copies: Vec<LcdTemplate>,
) -> IpcResponse {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    struct Running;
    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.store(false, Ordering::Release);
        }
    }
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return IpcResponse::error("A copied-template save is already running. Retry shortly");
    }
    let running = Running;
    let path = state.lock().templates_path();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = std::thread::Builder::new().name("template-merge".into()).spawn(move || {
        let _running = running;
        let result = template_store::merge_user_templates(&path, &originals, &copies).and_then(|()| {
            tx.send(DaemonEvent::IpcUpdate).map_err(|_| anyhow::anyhow!("Templates were saved but reload was not confirmed. Reconnect and inspect settings"))
        });
        let _ = sender.send(result.map(|()| IpcResponse::ok(serde_json::json!(null))).unwrap_or_else(|error| IpcResponse::error(format!("{error:#}"))));
    }) { return IpcResponse::error(format!("Could not start copied-template save: {error}")); }
    // A blocked filesystem retains the worker and write permit after the response deadline.
    receiver.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|_| IpcResponse::error("Copied-template save is unconfirmed and may still complete. Inspect templates before retrying"))
}
