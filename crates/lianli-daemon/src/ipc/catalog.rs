use std::path::PathBuf;
use std::sync::mpsc::Sender;

use lianli_shared::ipc::IpcResponse;
use lianli_shared::template::catalog::{self, CatalogTemplate};
use tracing::info;

use crate::ipc::SharedState;
use crate::service::DaemonEvent;
use crate::template_store;

pub fn install(
    state: &SharedState,
    tx: Sender<DaemonEvent>,
    template: CatalogTemplate,
) -> IpcResponse {
    let config_dir = {
        let s = state.lock();
        s.config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/var/lib/lianli"))
    };
    let sensors = lianli_shared::sensors::enumerate_sensors();

    let installed = match catalog::install_template(&template, &sensors, &config_dir) {
        Ok(t) => t,
        Err(e) => return IpcResponse::error(format!("install failed: {e}")),
    };

    let mut s = state.lock();
    let path = s.templates_path();
    let mut templates = template_store::load_user_templates(&path);
    if let Some(slot) = templates.iter_mut().find(|t| t.id == installed.id) {
        *slot = installed.clone();
    } else {
        templates.push(installed.clone());
    }
    if let Err(e) = template_store::save_user_templates(&path, &templates) {
        return IpcResponse::error(format!("installed but failed to persist: {e}"));
    }
    s.user_templates = templates;
    drop(s);

    let _ = tx.send(DaemonEvent::IpcUpdate);
    info!("installed catalog template '{}'", installed.id);
    IpcResponse::ok(&installed)
}
