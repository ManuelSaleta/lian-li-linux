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
    let destination = match scope {
        ServiceScope::User => caller.clone(),
        ServiceScope::System => Account::system()?,
    };
    let state = crate::destination::preflight(&destination, scope)?;
    ensure!(state.config_path == expected_config, "The selected daemon configuration differs from the destination service; reconnect to the intended daemon");
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
        source.context("Source import helper failed; inspect the destination before retrying")?;
    let destination = destination.context("Destination import helper failed; files may have been published, inspect storage before retrying")?;
    ensure!(
        source.status.success() && destination.status.success(),
        "Managed import failed; inspect storage before retrying. Source: {}. Destination: {}",
        source.stderr.chars().take(2048).collect::<String>(),
        destination.stderr.chars().take(2048).collect::<String>()
    );
    ensure!(source.stdout.is_empty(), "Unexpected source import output");
    let published: PublishedSelection = serde_json::from_str(&destination.stdout)
        .context("Invalid destination import result; inspect storage before retrying")?;
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
    let expected = parent.canonicalize()?.join("media/imports").join(id);
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
        };
        validate_result(&result, &config, id).unwrap();
        assert!(validate_result(&result, &config, "abcdef1234567890abcdef1234567890").is_err());
        result.lcds[0].path = Some(root.path().join("private.png"));
        assert!(validate_result(&result, &config, id).is_err());
        result.lcds[0].path = Some(path.join("../outside.png"));
        assert!(validate_result(&result, &config, id).is_err());
    }
}
