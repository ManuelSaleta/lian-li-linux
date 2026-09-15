use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceActionRequest, ServiceChangeRequest, ServiceOperationStatus};
use std::sync::{Mutex, OnceLock};
use tauri::{Emitter, Manager};

static STATUS: OnceLock<Mutex<ServiceOperationStatus>> = OnceLock::new();
static SUBMITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn state() -> &'static Mutex<ServiceOperationStatus> {
    STATUS.get_or_init(|| {
        Mutex::new(ServiceOperationStatus {
            active: false,
            message: String::new(),
            success: None,
        })
    })
}

pub fn status() -> ServiceOperationStatus {
    if !SUBMITTING.load(std::sync::atomic::Ordering::Acquire) {
        let observed = match observed_status() {
            Ok(Some(status)) => Some(status),
            Ok(None) => state()
                .lock()
                .unwrap()
                .active
                .then(|| ServiceOperationStatus {
                    active: false,
                    success: Some(false),
                    message:
                        "Service progress disappeared. Recheck actual services before retrying."
                            .into(),
                }),
            Err(error) => Some(ServiceOperationStatus {
                active: false,
                success: Some(false),
                message: format!("Cannot read service progress: {error:#}"),
            }),
        };
        if let Some(observed) = observed {
            let mut current = state().lock().unwrap();
            if !SUBMITTING.load(std::sync::atomic::Ordering::Acquire) {
                *current = observed;
            }
        }
    }
    state().lock().unwrap().clone()
}

fn observed_status() -> anyhow::Result<Option<ServiceOperationStatus>> {
    let action = lianli_control::operation_job::read()?;
    let change = if InstallationContext::detect() != InstallationContext::UnsupportedContainer {
        lianli_control::switch_job::read()?
    } else {
        None
    };
    Ok(match (action, change) {
        (Some(action), Some(change)) => {
            if change.status.active
                || (!action.status.active && change.updated_at_ms >= action.updated_at_ms)
            {
                Some(change.status)
            } else {
                Some(action.status)
            }
        }
        (Some(action), None) => Some(action.status),
        (None, Some(change)) => Some(change.status),
        (None, None) => None,
    })
}

pub fn active() -> bool {
    status().active
}

pub struct Operation {
    app: tauri::AppHandle,
    finished: bool,
}

impl Operation {
    pub fn begin(app: tauri::AppHandle) -> Result<Self, String> {
        if app.get_webview_window("editor").is_some() {
            return Err("Save and close the template editor before managing services.".into());
        }
        if status().active {
            return Err("Another service action is in progress.".into());
        }
        {
            let mut status = state().lock().unwrap();
            if status.active {
                return Err("Another service action is in progress.".into());
            }
            *status = ServiceOperationStatus {
                active: true,
                message: "Checking service state…".into(),
                success: None,
            };
            SUBMITTING.store(true, std::sync::atomic::Ordering::Release);
        }
        let operation = Self {
            app,
            finished: false,
        };
        operation.emit();
        Ok(operation)
    }

    fn emit(&self) {
        if let Err(error) = self
            .app
            .emit("service-operation", state().lock().unwrap().clone())
        {
            tracing::warn!("Cannot send service-operation progress: {error}");
        }
    }

    pub fn run(self, request: ServiceActionRequest) -> Result<(), String> {
        self.submit(|| {
            let id = lianli_control::operation_job::start(&InstallationContext::detect(), request)
                .map_err(|error| format!("{error:#}"))?;
            let record = lianli_control::operation_job::read()
                .map_err(|error| format!("Cannot observe service progress: {error:#}"))?
                .ok_or("Service progress disappeared. Recheck actual services before retrying.")?;
            if record.id != id {
                return Err(
                    "Another operation replaced this progress record. Recheck actual services."
                        .into(),
                );
            }
            Ok(record.status)
        })
    }

    pub fn run_change(self, request: ServiceChangeRequest) -> Result<(), String> {
        self.submit(|| {
            let id =
                lianli_control::switch_job::start(request).map_err(|error| format!("{error:#}"))?;
            let record = lianli_control::switch_job::read()
                .map_err(|error| format!("Cannot observe switch progress: {error:#}"))?
                .ok_or("Switch progress disappeared. Recheck before retrying.")?;
            if record.id != id {
                return Err(
                    "Another switch replaced the progress record. Recheck actual services.".into(),
                );
            }
            Ok(record.status)
        })
    }

    pub fn run_setup(
        self,
        deployment: lianli_control::container_deployment::Deployment,
    ) -> Result<(), String> {
        self.submit(|| {
            lianli_control::container_bootstrap::install(&deployment)
                .map_err(|error| format!("{error:#}"))?;
            Ok(ServiceOperationStatus {
                active: false,
                success: Some(true),
                message: "Host support installed. Choose which service mode to start.".into(),
            })
        })
    }

    fn submit(
        mut self,
        submit: impl FnOnce() -> Result<ServiceOperationStatus, String>,
    ) -> Result<(), String> {
        let result = submit().map(|status| *state().lock().unwrap() = status);
        if let Err(message) = &result {
            let mut status = state().lock().unwrap();
            status.active = false;
            status.success = Some(false);
            status.message = message.clone();
        }
        SUBMITTING.store(false, std::sync::atomic::Ordering::Release);
        self.finished = true;
        self.emit();
        result
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if !self.finished {
            SUBMITTING.store(false, std::sync::atomic::Ordering::Release);
            *state().lock().unwrap() = ServiceOperationStatus {
                active: false,
                message: "GUI submission was interrupted. Recheck independent service progress before retrying."
                    .into(),
                success: Some(false),
            };
            self.emit();
        }
    }
}
