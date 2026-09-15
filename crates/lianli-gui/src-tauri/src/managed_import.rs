use anyhow::{ensure, Result};
use lianli_shared::{
    config::LcdConfig,
    daemon::{DaemonInfo, DaemonMode},
    services::ServiceScope,
    template::LcdTemplate,
};
use std::sync::atomic::{AtomicBool, Ordering};

static SUBMITTING: AtomicBool = AtomicBool::new(false);
struct Submission;
impl Drop for Submission {
    fn drop(&mut self) {
        SUBMITTING.store(false, Ordering::Release);
    }
}

fn connected(expected: &str) -> Result<DaemonInfo> {
    let info: DaemonInfo = serde_json::from_value(
        crate::ipc::request("GetDaemonInfo", serde_json::Value::Null)
            .map_err(anyhow::Error::msg)?,
    )?;
    ensure!(
        info.instance_id == expected,
        "The connected daemon changed. Review the selection before copying"
    );
    Ok(info)
}

pub fn start(instance: &str, lcds: Vec<LcdConfig>, templates: Vec<LcdTemplate>) -> Result<String> {
    ensure!(
        SUBMITTING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "An import submission is already pending"
    );
    let _submission = Submission;
    ensure!(
        !crate::service_operations::active(),
        "Wait for the service operation before importing media"
    );
    let info = connected(instance)?;
    ensure!(
        templates.is_empty()
            || info
                .capabilities
                .iter()
                .any(|capability| capability == "managed_template_merge"),
        "Update the daemon before copying custom-template assets"
    );
    let scope = match info.mode {
        DaemonMode::User => ServiceScope::User,
        DaemonMode::System => ServiceScope::System,
        DaemonMode::Unknown => {
            anyhow::bail!("The daemon mode is unknown. Reconnect before importing")
        }
    };
    lianli_control::media_import_worker::start(scope, lcds, templates, &info.config_path, instance)
}

pub fn status() -> Result<Option<lianli_control::media_import_job::Status>> {
    lianli_control::media_import_worker::read()
}

pub fn result(
    instance: &str,
    id: &str,
) -> Result<lianli_control::media_import::PublishedSelection> {
    let info = connected(instance)?;
    let status = status()?.ok_or_else(|| anyhow::anyhow!("No import result is recorded"))?;
    ensure!(
        !status.active && status.id == id,
        "This import is active or was replaced. Check progress again"
    );
    let result = status
        .result
        .ok_or_else(|| anyhow::anyhow!("The import has no verified result"))?;
    lianli_control::media_import_launch::validate_result(&result, &info.config_path, id)?;
    Ok(result)
}
