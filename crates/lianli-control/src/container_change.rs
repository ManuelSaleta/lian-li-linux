use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::ServiceChangeRequest;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

struct CachedProgress {
    fingerprint: (u64, u64, i64, i64, u64),
    checked: Instant,
    status: Option<crate::switch_job::JobStatus>,
}

static PROGRESS: Mutex<Option<CachedProgress>> = Mutex::new(None);

const DISPATCH: &str = "for helper in /usr/bin/lianli-control /usr/local/libexec/lianli/lianli-control; do if [ -e \"$helper\" ] || [ -L \"$helper\" ]; then exec \"$helper\" \"$@\"; fi; done; echo 'The Distrobox host helper is missing' >&2; exit 127";

fn call(arguments: &[String], timeout: Duration) -> Result<crate::command::Output> {
    let context = InstallationContext::detect();
    ensure!(
        matches!(context, InstallationContext::Distrobox { .. }),
        "Use the host bridge from a supported Distrobox"
    );
    let route = crate::services::Route::detect(&context)?;
    let mut args = vec!["-c", DISPATCH, "lianli-host-switch"];
    args.extend(arguments.iter().map(String::as_str));
    route.bounded_output("/usr/bin/sh", &args, timeout)
}

fn box_name() -> Result<String> {
    match InstallationContext::detect() {
        InstallationContext::Distrobox { name } => Ok(name),
        _ => anyhow::bail!("Container switching requires a supported Distrobox"),
    }
}

fn arguments(name: &str, request: ServiceChangeRequest) -> Vec<String> {
    let mut args = vec![
        "request-switch".into(),
        "--expected-box".into(),
        name.into(),
    ];
    args.extend(crate::switch_job::request_arguments(request));
    args
}

pub fn start(request: ServiceChangeRequest) -> Result<String> {
    let args = arguments(&box_name()?, request);
    let output = call(&args, Duration::from_secs(220))
        .context("Host switch submission was not confirmed. Recheck progress before retrying")?;
    ensure!(
        output.status.success(),
        "Host switch submission failed: {}",
        output.stderr.trim()
    );
    let id: String =
        serde_json::from_str(&output.stdout).context("Invalid host switch response")?;
    lianli_shared::daemon::parse_service_invocation(&id).map_err(anyhow::Error::msg)?;
    Ok(id)
}

pub fn read() -> Result<Option<crate::switch_job::JobStatus>> {
    let metadata = match std::fs::symlink_metadata("/run/host/run/lianli-switch/status.json") {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            *PROGRESS.lock().unwrap() = None;
            return Ok(None);
        }
        Err(error) => return Err(error).context("Cannot inspect host switch progress"),
    };
    let fingerprint = (
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.len(),
    );
    {
        let cache = PROGRESS.lock().unwrap();
        if let Some(cache) = cache.as_ref() {
            let lifetime = if cache
                .status
                .as_ref()
                .is_some_and(|status| status.status.active)
            {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(60)
            };
            if cache.fingerprint == fingerprint && cache.checked.elapsed() < lifetime {
                return Ok(cache.status.clone());
            }
        }
    }
    let output = call(
        &["switch-status".into(), "--expected-box".into(), box_name()?],
        Duration::from_secs(4),
    )?;
    ensure!(
        output.status.success(),
        "Cannot read host switch progress: {}",
        output.stderr.trim()
    );
    let status: Option<crate::switch_job::JobStatus> =
        serde_json::from_str(&output.stdout).context("Invalid host switch progress")?;
    *PROGRESS.lock().unwrap() = Some(CachedProgress {
        fingerprint,
        checked: Instant::now(),
        status: status.clone(),
    });
    Ok(status)
}

pub fn verify_host_request(name: &str) -> Result<()> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Handle container switch requests on the host"
    );
    let account = crate::account::Account::user(unsafe { libc::geteuid() })?;
    let deployment = crate::container_deployment::load()?
        .context("Set up Distrobox services before switching modes")?;
    deployment.verify_owner(&account)?;
    ensure!(
        deployment.route.launch.name == name,
        "The host deployment belongs to another Distrobox"
    );
    crate::host_helper::installed()?;
    Ok(())
}

pub fn verify_services() -> Result<()> {
    let output = call(
        &[
            "check-container-services".into(),
            "--expected-box".into(),
            box_name()?,
        ],
        Duration::from_secs(20),
    )?;
    ensure!(
        output.status.success(),
        "Container service setup is unverified: {}",
        output.stderr.trim()
    );
    Ok(())
}

pub fn deployment() -> Result<Option<crate::container_deployment::Deployment>> {
    if !std::path::Path::new("/run/host/etc/lianli-control/distrobox.json").try_exists()? {
        return Ok(None);
    }
    let output = call(
        &[
            "read-container-deployment".into(),
            "--expected-box".into(),
            box_name()?,
        ],
        Duration::from_secs(4),
    )?;
    ensure!(
        output.status.success(),
        "Cannot inspect the existing host deployment: {}",
        output.stderr.trim()
    );
    serde_json::from_str(&output.stdout).context("Invalid host deployment response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::services::ServiceScope;

    #[test]
    fn bridge_requests_preserve_box_names_and_typed_options_as_separate_arguments() {
        assert_eq!(
            arguments(
                "box with spaces",
                ServiceChangeRequest::Switch {
                    scope: ServiceScope::System,
                    carry_settings: true
                }
            ),
            [
                "request-switch",
                "--expected-box",
                "box with spaces",
                "--scope",
                "system",
                "--carry-settings"
            ]
        );
        assert_eq!(
            arguments("box", ServiceChangeRequest::Recover {}),
            ["request-switch", "--expected-box", "box", "--recover"]
        );
    }
}
