use crate::{account::Account, media_import::PublishedSelection, transfer_channel::Channel};
use anyhow::{ensure, Context, Result};
use lianli_shared::{installation::InstallationContext, services::ServiceScope};
use std::{ffi::OsStr, path::Path, process::Stdio, time::Duration};

pub fn native(
    scope: ServiceScope,
    selection: &Path,
    expected_config: &Path,
    id: &str,
) -> Result<PublishedSelection> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Managed import launching requires the native host application"
    );
    ensure!(
        selection.is_absolute()
            && selection.as_os_str().len() <= 4096
            && expected_config.is_absolute(),
        "Select absolute input and destination paths"
    );
    ensure!(
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid managed import ID"
    );
    let caller = Account::authorized_caller()?;
    let destination = match crate::container_deployment::load()? {
        Some(deployment) => {
            deployment.verify_installed(&caller)?;
            let (config, working) = deployment.route.paths(scope);
            ensure!(
                config == expected_config,
                "The selected configuration differs from the installed Distrobox destination"
            );
            let (execution, _) = crate::container_destination::Execution::discover(
                &caller,
                deployment.route.launch.clone(),
                scope,
                config,
                working,
                false,
            )?;
            caller.clone().with_container(execution)?
        }
        None => match scope {
            ServiceScope::User => caller.clone(),
            ServiceScope::System => Account::system()?,
        },
    };
    let state = crate::destination::preflight(&destination, scope)?;
    ensure!(state.config_path == expected_config, "The selected daemon configuration differs from the destination service. Reconnect to the intended daemon");
    let source_command = caller.control_command(&[
        OsStr::new("send-selected-media"),
        OsStr::new("--selection"),
        selection.as_os_str(),
    ])?;
    let destination_command = destination.control_command(&[
        OsStr::new("receive-selected-media"),
        OsStr::new("--scope"),
        OsStr::new(match scope {
            ServiceScope::User => "user",
            ServiceScope::System => "system",
        }),
        OsStr::new("--operation-id"),
        OsStr::new(id),
        OsStr::new("--expected-config"),
        expected_config.as_os_str(),
    ])?;
    let (source_fd, destination_fd) = Channel::pair()?;
    let (source, destination) = std::thread::scope(|threads| {
        let source = threads.spawn(move || {
            crate::command::run_with_stdin(
                source_command,
                Stdio::from(source_fd),
                Duration::from_secs(600),
            )
        });
        let destination = crate::command::run_with_stdin_limit(
            destination_command,
            Stdio::from(destination_fd),
            Duration::from_secs(600),
            16 * 1024 * 1024,
        );
        (
            source
                .join()
                .map_err(|_| anyhow::anyhow!("Source import supervisor panicked"))
                .and_then(|result| result),
            destination,
        )
    });
    let source =
        source.context("Source import failed. Inspect destination storage before retrying")?;
    let destination = destination.context(
        "Destination import failed. Files may have been copied. Inspect storage before retrying",
    )?;
    ensure!(
        source.status.success() && destination.status.success(),
        "Managed import failed. Inspect storage before retrying. Source: {}. Destination: {}",
        source.stderr.chars().take(2048).collect::<String>(),
        destination.stderr.chars().take(2048).collect::<String>()
    );
    ensure!(source.stdout.is_empty(), "Unexpected source import output");
    let published: PublishedSelection = serde_json::from_str(&destination.stdout)
        .context("Invalid destination import result. Inspect storage before retrying")?;
    validate_result(&published, expected_config, id)?;
    Ok(published)
}

pub fn validate_result(result: &PublishedSelection, config: &Path, id: &str) -> Result<()> {
    ensure!(
        result.import_id == id,
        "Destination returned a different import identity"
    );
    let parent = config
        .parent()
        .context("Destination configuration has no parent")?;
    crate::media_ownership::validate_id(id)?;
    let directory = match &result.destination {
        Some(destination) => {
            // The publishing worker resolves this path in the daemon's filesystem namespace.
            ensure!(
                destination.config_path == config,
                "Import result belongs to another configuration"
            );
            crate::container_destination::absolute(&destination.state_directory)?;
            destination.state_directory.clone()
        }
        None => parent.canonicalize()?,
    };
    let expected = directory.join("media/imports").join(id);
    let dependencies = crate::media_import::selection_dependencies_with_limit(
        &result.lcds,
        &result.templates,
        16 * 1024 * 1024,
    )?;
    ensure!(
        dependencies
            .iter()
            .all(|dependency| dependency.path.parent() == Some(expected.as_path())),
        "Destination returned paths outside the authorized managed import"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_result_validation_does_not_require_host_visibility() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("guest-only/state");
        let config = root.path().join("guest-alias/config.json");
        let id = "1234567890abcdef1234567890abcdef";
        let path = directory.join("media/imports").join(id).join("asset.png");
        let mut result = PublishedSelection {
            import_id: id.into(),
            lcds: vec![
                serde_json::from_value(serde_json::json!({"type":"image", "path":path})).unwrap(),
            ],
            templates: vec![],
            destination: Some(crate::media_import::PublishedDestination {
                config_path: config.clone(),
                state_directory: directory.clone(),
            }),
        };
        assert!(!directory.exists());
        result = serde_json::from_slice(&serde_json::to_vec(&result).unwrap()).unwrap();
        validate_result(&result, &config, id).unwrap();
        assert!(validate_result(&result, &directory.join("other.json"), id).is_err());
        result.lcds[0].path = Some(directory.join("private.png"));
        assert!(validate_result(&result, &config, id).is_err());
        result.lcds[0].path = Some(path);
        result.destination.as_mut().unwrap().state_directory = directory.join("../outside");
        assert!(validate_result(&result, &config, id).is_err());
        let legacy: PublishedSelection = serde_json::from_value(serde_json::json!({
            "import_id": id, "lcds": [], "templates": []
        }))
        .unwrap();
        assert!(legacy.destination.is_none());
    }

    #[test]
    fn result_must_match_the_requested_import_and_destination() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let id = "1234567890abcdef1234567890abcdef";
        let path = root
            .path()
            .canonicalize()
            .unwrap()
            .join("media/imports")
            .join(id)
            .join("asset.png");
        let mut result = PublishedSelection {
            import_id: id.into(),
            lcds: vec![
                serde_json::from_value(serde_json::json!({"type":"image", "path":path})).unwrap(),
            ],
            templates: vec![],
            destination: None,
        };
        validate_result(&result, &config, id).unwrap();
        assert!(validate_result(&result, &config, "abcdef1234567890abcdef1234567890").is_err());
        result.lcds[0].path = Some(root.path().join("private.png"));
        assert!(validate_result(&result, &config, id).is_err());
        result.lcds[0].path = Some(path.join("../outside.png"));
        assert!(validate_result(&result, &config, id).is_err());
    }
}
