use crate::media_import::{self, PreparedSelection};
use crate::media_staging::CopyControl;
use crate::transfer_channel::Channel;
use anyhow::{ensure, Context, Result};
use lianli_shared::{config::LcdConfig, template::LcdTemplate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn send_file(selection: &Path) -> Result<()> {
    let (lcds, templates) = read_selection(selection)?;
    send(
        std::io::stdin().as_fd().try_clone_to_owned()?,
        lcds,
        templates,
        &CopyControl::new(Duration::from_secs(600)),
    )
}

pub(crate) fn read_selection(selection: &Path) -> Result<(Vec<LcdConfig>, Vec<LcdTemplate>)> {
    use std::io::Read;
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Read selections under the source account"
    );
    let mut bytes = Vec::new();
    crate::state::open_media_source(selection)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "Selection input exceeds 1 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn receive_published(
    scope: lianli_shared::services::ServiceScope,
    import_id: &str,
    expected_config: &Path,
) -> Result<media_import::PublishedSelection> {
    use lianli_shared::installation::InstallationContext;
    crate::container_destination::verify_config(expected_config)?;
    ensure!(
        import_id.len() == 32 && import_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid import ID"
    );
    let destination = crate::destination::inspect(scope, false)?;
    ensure!(
        destination.config_path == expected_config,
        "Managed import destination differs from the reviewed configuration path"
    );
    let parent = destination
        .config_path
        .parent()
        .context("Destination state directory missing")?;
    let final_path = parent.canonicalize()?.join("media/imports").join(import_id);
    let control = CopyControl::new(Duration::from_secs(600));
    let prepared = receive(
        std::io::stdin().as_fd().try_clone_to_owned()?,
        parent,
        &final_path,
        &control,
    )?;
    let validated = prepared.validate(&control)?;
    let operation =
        crate::reservation::ServiceOperationLock::acquire(&InstallationContext::detect())?;
    operation.verify()?;
    let current = crate::destination::inspect(scope, false)?;
    ensure!(
        current.uid == destination.uid
            && current.config_path == destination.config_path
            && current.groups_fingerprint == destination.groups_fingerprint
            && current.mount_namespace == destination.mount_namespace,
        "Managed import destination changed after preparation"
    );
    let mut published = validated.publish(parent, import_id, &control)?;
    published.destination = Some(media_import::PublishedDestination {
        config_path: expected_config.into(),
        state_directory: parent.canonicalize()?,
    });
    Ok(published)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Message {
    Selection { bytes: usize, sha256: String },
    Asset { path: PathBuf },
    Complete,
}

/// Use a supervised source-account process and an inherited private packet channel.
pub fn send(
    fd: OwnedFd,
    lcds: Vec<LcdConfig>,
    templates: Vec<LcdTemplate>,
    control: &CopyControl,
) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Open selected sources under an unprivileged source account"
    );
    control.check()?;
    let dependencies = media_import::selection_dependencies(&lcds, &templates)?;
    let channel = Channel::new(fd, Duration::from_secs(600))?;
    let bytes = serde_json::to_vec(&(lcds, templates))?;
    let sealed = crate::state_transfer::sealed_state(&bytes)?;
    channel.send(
        &Message::Selection {
            bytes: bytes.len(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        },
        Some(sealed.as_fd()),
    )?;
    for dependency in dependencies {
        control.check()?;
        let source = crate::state::open_media_source(&dependency.path)
            .with_context(|| format!("Opening selected source for {}", dependency.owner))?;
        channel.send(
            &Message::Asset {
                path: dependency.path,
            },
            Some(source.as_fd()),
        )?;
    }
    let (message, descriptor) = channel.receive::<Message>()?;
    ensure!(
        matches!(message, Message::Complete) && descriptor.is_none(),
        "Selection transfer did not complete"
    );
    control.check()
}

/// Run under the destination account; source paths are labels and are never opened here.
pub fn receive(
    fd: OwnedFd,
    staging_parent: &Path,
    destination: &Path,
    control: &CopyControl,
) -> Result<PreparedSelection> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Copy selected media under its unprivileged destination account"
    );
    control.check()?;
    let channel = Channel::new(fd, Duration::from_secs(600))?;
    let (message, descriptor) = channel.receive::<Message>()?;
    let Message::Selection { bytes, sha256 } = message else {
        anyhow::bail!("Expected a sealed media selection")
    };
    ensure!(
        bytes <= 1024 * 1024 && sha256.len() == 64,
        "Invalid media selection size or checksum"
    );
    let bytes = crate::state_transfer::read_sealed_state(
        descriptor.context("Selection descriptor missing")?,
        bytes,
        &sha256,
    )?;
    let (lcds, templates) = serde_json::from_slice(&bytes)?;
    let prepared = media_import::prepare_from(
        lcds,
        templates,
        staging_parent,
        destination,
        control,
        |expected| {
            control.check()?;
            let (message, descriptor) = channel.receive::<Message>()?;
            ensure!(
                matches!(message, Message::Asset { path } if path == expected),
                "Unexpected media descriptor order or path"
            );
            Ok(std::fs::File::from(
                descriptor.context("Media descriptor missing")?,
            ))
        },
    )?;
    control.check()?;
    channel.send(&Message::Complete, None)?;
    Ok(prepared)
}

#[cfg(test)]
#[path = "media_import_transfer_tests.rs"]
mod tests;
