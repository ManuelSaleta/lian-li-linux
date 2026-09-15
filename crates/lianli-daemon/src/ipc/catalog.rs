use super::EventSender;
use std::path::PathBuf;

use lianli_shared::ipc::IpcResponse;
use lianli_shared::template::catalog::CatalogInstallStatus;
use lianli_shared::template::catalog::{self, CatalogTemplate};
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc,
};
use tracing::info;

use crate::ipc::SharedState;
use crate::service::DaemonEvent;
use crate::template_store;

type Job = Arc<Mutex<Option<CatalogInstallStatus>>>;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const STOPPED: u8 = 1;
const PUBLISHING: u8 = 2;
static INSPECTING: AtomicBool = AtomicBool::new(false);

struct Inspection;
impl Drop for Inspection {
    fn drop(&mut self) {
        INSPECTING.store(false, Ordering::Release);
    }
}

pub fn storage(state: &SharedState, managed: bool) -> IpcResponse {
    if INSPECTING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return IpcResponse::error("Media storage inspection is already running. Retry shortly");
    }
    let inspection = Inspection;
    let runtime = state.lock().catalog_runtime.clone();
    let locations = {
        let state = state.lock();
        crate::state_backups::Locations {
            config: state.config_path.clone(),
            templates: state.templates_path(),
            presets: state.presets_path.clone(),
        }
    };
    let directory = locations
        .config
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = std::thread::Builder::new()
        .name("catalog-storage".into())
        .spawn(move || {
            let _inspection = inspection;
            let inventory = if managed {
                lianli_control::media_ownership::inspect(&directory)
            } else {
                catalog::inspect_storage(&directory)
            };
            let result = inventory
                .and_then(|mut entries| {
                    crate::catalog_references::inspect(&locations, &mut entries)?;
                    runtime.inspect(&mut entries)?;
                    Ok(entries)
                })
                .map(|entries| IpcResponse::ok(&entries))
                .unwrap_or_else(|error| {
                    IpcResponse::error(format!("Media storage inventory incomplete: {error:#}"))
                });
            let _ = sender.send(result);
        })
    {
        return IpcResponse::error(format!("Could not inspect media storage: {error}"));
    }
    // Keep one inspection slot occupied if filesystem I/O outlasts the client deadline.
    receiver.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|_| IpcResponse::error("Media inspection timed out. Cleanup remains disabled. Check storage access and retry."))
}

#[derive(Default)]
pub struct CatalogControl(AtomicU8);

impl CatalogControl {
    pub fn stop(&self) {
        self.0.fetch_or(STOPPED, Ordering::AcqRel);
    }

    fn stopped(&self) -> bool {
        self.0.load(Ordering::Acquire) & STOPPED != 0
    }

    fn publish(&self) -> Option<Publication<'_>> {
        self.0
            .compare_exchange(0, PUBLISHING, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Publication { control: self })
    }
}

struct Publication<'a> {
    control: &'a CatalogControl,
}
impl Drop for Publication<'_> {
    fn drop(&mut self) {
        self.control.0.fetch_and(STOPPED, Ordering::AcqRel);
    }
}

fn begin(
    state: &SharedState,
    template: &CatalogTemplate,
) -> Result<(Job, CatalogInstallStatus), IpcResponse> {
    if template.id.len() > 128 {
        return Err(IpcResponse::error("Catalog template ID exceeds 128 bytes"));
    }
    let (job, instance) = {
        let state = state.lock();
        if state.catalog_control.stopped() {
            return Err(IpcResponse::error(
                "The daemon is stopping. Catalog installation is unavailable",
            ));
        }
        (
            state.catalog_install.clone(),
            state.info.instance_id.clone(),
        )
    };
    let mut current = job.lock();
    if current.as_ref().is_some_and(|status| !status.finished) {
        return Err(IpcResponse::error(
            "A catalog installation is already running. Check its status before retrying.",
        ));
    }
    let status = CatalogInstallStatus {
        operation_id: format!("{instance}:{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        template_id: template.id.clone(),
        finished: false,
        error: None,
    };
    *current = Some(status.clone());
    drop(current);
    Ok((job, status))
}

fn finish(job: &Job, response: &IpcResponse) {
    if let Some(status) = job.lock().as_mut() {
        status.finished = true;
        status.error = match response {
            IpcResponse::Error { message } => Some(message.chars().take(2048).collect()),
            IpcResponse::Ok { .. } => None,
        };
    }
}

pub fn status(state: &SharedState) -> IpcResponse {
    let job = state.lock().catalog_install.clone();
    let status = job.lock().clone();
    IpcResponse::ok(&status)
}

pub fn start(state: &SharedState, tx: EventSender, template: CatalogTemplate) -> IpcResponse {
    let (job, initial) = match begin(state, &template) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let worker_job = job.clone();
    let state = state.clone();
    // Detach so stalled filesystem cleanup cannot delay hardware teardown; retain the write permit.
    let spawned = std::thread::Builder::new()
        .name("catalog-install".into())
        .spawn(move || {
            let response = run(&state, tx, template);
            finish(&worker_job, &response);
        });
    if let Err(error) = spawned {
        let response = IpcResponse::error(format!("Could not start catalog installation: {error}"));
        finish(&job, &response);
        return response;
    }
    IpcResponse::ok(&initial)
}

pub fn install(state: &SharedState, tx: EventSender, template: CatalogTemplate) -> IpcResponse {
    let (job, _) = match begin(state, &template) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let response = run(state, tx, template);
    finish(&job, &response);
    response
}

fn run(state: &SharedState, tx: EventSender, template: CatalogTemplate) -> IpcResponse {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install_inner(state, tx, template)))
        .unwrap_or_else(|_| IpcResponse::error("Catalog worker stopped unexpectedly. Reload templates before retrying. Publication may have completed."))
}

fn install_inner(state: &SharedState, tx: EventSender, template: CatalogTemplate) -> IpcResponse {
    let control = state.lock().catalog_control.clone();
    if control.stopped() {
        return IpcResponse::error("Catalog installation cancelled during daemon shutdown");
    }
    let config_dir = {
        let s = state.lock();
        s.config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/var/lib/lianli"))
    };
    let sensors = lianli_shared::sensors::enumerate_sensors();

    let prepared =
        match catalog::install_template(&template, &sensors, &config_dir, || control.stopped()) {
            Ok(t) => t,
            Err(e) => return IpcResponse::error(format!("install failed: {e}")),
        };
    let installed = prepared.template();

    let path = state.lock().templates_path();
    let Some(_publication) = control.publish() else {
        return IpcResponse::error(
            "Catalog installation cancelled before saving during daemon shutdown",
        );
    };
    if let Err(e) = template_store::install_user_template(&path, installed) {
        state
            .lock()
            .state_health
            .templates_failed(&format!("{e:#}"));
        prepared.commit();
        return IpcResponse::error(format!("Template save could not be confirmed: {e}. Verified assets were retained because publication may have completed. Reload templates before retrying."));
    }
    let installed = prepared.commit();

    if tx.send(DaemonEvent::IpcUpdate).is_err() {
        return IpcResponse::error("Template was saved, but the daemon stopped before reload. Reconnect and review templates before retrying.");
    }
    info!("installed catalog template '{}'", installed.id);
    IpcResponse::ok(&installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_prevents_publication_and_cannot_be_cleared_by_finishing_a_save() {
        let control = CatalogControl::default();
        let publication = control.publish().unwrap();
        assert!(control.publish().is_none());
        control.stop();
        assert!(control.stopped());
        drop(publication);
        assert!(control.stopped());
        assert!(control.publish().is_none());

        let control = CatalogControl::default();
        drop(control.publish().unwrap());
        assert!(control.publish().is_some());
        control.stop();
        assert!(control.publish().is_none());
    }

    #[test]
    fn install_status_serializes_work_bounds_errors_and_allows_explicit_retry() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(crate::ipc::DaemonState::new(
            root.path().join("config.json"),
        )));
        let manifest: catalog::CatalogManifest =
            serde_json::from_str(include_str!("../../../../templates/default_templates.json"))
                .unwrap();
        let template = &manifest.templates[0];
        let (job, first) = begin(&state, template).unwrap();
        assert!(!first.finished);
        assert!(begin(&state, template).is_err());
        assert!(
            matches!(status(&state), IpcResponse::Ok { data } if data["operation_id"] == first.operation_id)
        );
        finish(&job, &IpcResponse::error("x".repeat(4096)));
        let failed = job.lock().clone().unwrap();
        assert!(failed.finished);
        assert_eq!(failed.error.unwrap().len(), 2048);
        let (_, retry) = begin(&state, template).unwrap();
        assert_ne!(first.operation_id, retry.operation_id);
        finish(&job, &IpcResponse::ok(serde_json::Value::Null));
        assert!(job.lock().as_ref().unwrap().finished);
        assert!(job.lock().as_ref().unwrap().error.is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        state.lock().catalog_control.stop();
        assert!(begin(&state, template).is_err());
    }

    #[test]
    fn install_start_is_guarded_but_status_is_read_only() {
        let manifest: catalog::CatalogManifest =
            serde_json::from_str(include_str!("../../../../templates/default_templates.json"))
                .unwrap();
        assert!(!lianli_shared::ipc::IpcRequest::StartCatalogInstall {
            template: manifest.templates[0].clone()
        }
        .is_read_only());
        assert!(lianli_shared::ipc::IpcRequest::GetCatalogInstallStatus.is_read_only());
    }
}
