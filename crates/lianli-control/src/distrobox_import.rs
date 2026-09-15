use crate::media_import::{self, PublishedSelection};
use crate::media_publication::Directory;
use crate::media_staging::CopyControl;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::{DaemonInfo, DaemonMode, FileIdentity, SERVICE_WRITE_GATE};
use lianli_shared::installation::InstallationContext;
use lianli_shared::ipc::IpcRequest;
use lianli_shared::services::ServiceScope;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

fn validate_peer(
    info: &DaemonInfo,
    peer: &libc::ucred,
    config: &Path,
    instance: &str,
    uid: u32,
    same_namespace: bool,
    scope: ServiceScope,
) -> Result<()> {
    ensure!(
        uid != 0 && peer.uid == uid && peer.pid > 0 && info.pid == peer.pid as u32,
        "Distrobox import requires the connected daemon under the same desktop account"
    );
    ensure!(same_namespace, "The import worker and daemon have different filesystem namespaces. Start both inside the same box");
    ensure!(
        info.mode
            == match scope {
                ServiceScope::User => DaemonMode::User,
                ServiceScope::System => DaemonMode::System,
            }
            && info.config_path == config
            && info.instance_id == instance,
        "The selected Distrobox daemon changed. Reconnect and review the import again"
    );
    info.write_guard(env!("CARGO_PKG_VERSION"))
        .map_err(anyhow::Error::msg)?;
    ensure!(
        info.capabilities
            .iter()
            .any(|capability| capability == SERVICE_WRITE_GATE),
        "Update the daemon before copying managed media"
    );
    Ok(())
}

fn connected(
    context: &InstallationContext,
    config: &Path,
    instance: &str,
    scope: ServiceScope,
) -> Result<DaemonInfo> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut stream, peer) =
        crate::daemon_probe::connect(&crate::daemon_probe::socket_path(context, scope)?, deadline)?;
    ensure!(
        peer.pid > 0,
        "Daemon is not visible in this process namespace"
    );
    let info: DaemonInfo = serde_json::from_value(crate::daemon_probe::send_request(
        &mut stream,
        &IpcRequest::GetDaemonInfo,
        deadline,
        64 * 1024,
    )?)?;
    let own_namespace = fs::metadata("/proc/self/ns/mnt")?;
    let peer_namespace = fs::metadata(format!("/proc/{}/ns/mnt", peer.pid))?;
    validate_peer(
        &info,
        &peer,
        config,
        instance,
        unsafe { libc::geteuid() },
        (own_namespace.dev(), own_namespace.ino()) == (peer_namespace.dev(), peer_namespace.ino()),
        scope,
    )?;
    Ok(info)
}

fn directory_identity(directory: &Directory) -> Result<FileIdentity> {
    let metadata = directory.0.metadata()?;
    Ok(FileIdentity {
        device: metadata.dev().to_string(),
        inode: metadata.ino().to_string(),
    })
}

pub fn copy(
    scope: ServiceScope,
    selection: &Path,
    config: &Path,
    instance: &str,
    id: &str,
) -> Result<PublishedSelection> {
    let context = InstallationContext::detect();
    ensure!(
        matches!(context, InstallationContext::Distrobox { .. }),
        "Run this import inside its Distrobox"
    );
    ensure!(
        config.is_absolute()
            && config.as_os_str().len() <= 4096
            && !instance.is_empty()
            && instance.len() <= 256,
        "Invalid managed import destination"
    );
    crate::media_ownership::validate_id(id)?;
    let initial = connected(&context, config, instance, scope)?;
    {
        let operation = crate::reservation::ServiceOperationLock::acquire(&context)?;
        ensure!(
            initial.service_operation_lock.as_ref() == Some(operation.identity()),
            "Daemon and import worker do not share the verified host write lock"
        );
    }
    let parent = config
        .parent()
        .context("Managed destination has no parent")?;
    let directory = Directory::open(parent)?;
    let identity = directory_identity(&directory)?;
    check_transaction(&directory)?;
    let (lcds, templates) = crate::media_import_transfer::read_selection(selection)?;
    let control = CopyControl::new(Duration::from_secs(600));
    let destination = parent.canonicalize()?.join("media/imports").join(id);
    let prepared =
        media_import::prepare_selection(lcds, templates, parent, &destination, &control)?
            .validate(&control)?;
    let operation = crate::reservation::ServiceOperationLock::acquire(&context)?;
    operation.verify()?;
    let current = connected(&context, config, instance, scope)?;
    ensure!(
        initial.service_operation_lock.as_ref() == Some(operation.identity())
            && current.service_operation_lock.as_ref() == Some(operation.identity()),
        "Daemon and import worker do not share the verified host write lock"
    );
    let current_directory = Directory::open(parent)?;
    ensure!(
        directory_identity(&current_directory)? == identity,
        "Managed destination directory was replaced"
    );
    check_transaction(&current_directory)?;
    let mut published = prepared.publish(parent, id, &control)?;
    published.destination = Some(crate::media_import::PublishedDestination {
        config_path: config.into(),
        state_directory: parent.canonicalize()?,
    });
    Ok(published)
}

fn check_transaction(directory: &Directory) -> Result<()> {
    match fs::symlink_metadata(directory.path().join(".lianli-state-transaction.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Checking destination recovery state"),
        Ok(_) => anyhow::bail!("Recover the interrupted state transaction before importing media"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate_peer(
        info: &DaemonInfo,
        peer: &libc::ucred,
        config: &Path,
        instance: &str,
        uid: u32,
        same_namespace: bool,
    ) -> Result<()> {
        super::validate_peer(
            info,
            peer,
            config,
            instance,
            uid,
            same_namespace,
            ServiceScope::User,
        )
    }

    #[test]
    fn import_refuses_another_account_namespace_instance_or_mode() {
        let info = DaemonInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: lianli_shared::daemon::IPC_PROTOCOL_VERSION,
            instance_id: "selected".into(),
            pid: 42,
            mode: DaemonMode::User,
            config_path: "/state/config.json".into(),
            capabilities: vec![
                lianli_shared::daemon::GUARDED_WRITES.into(),
                SERVICE_WRITE_GATE.into(),
            ],
            ownership_lock: None,
            service_invocation: None,
            service_operation_lock: None,
        };
        let peer = libc::ucred {
            pid: 42,
            uid: 1000,
            gid: 1000,
        };
        validate_peer(&info, &peer, &info.config_path, "selected", 1000, true).unwrap();
        assert!(validate_peer(&info, &peer, &info.config_path, "selected", 1001, true).is_err());
        assert!(validate_peer(&info, &peer, &info.config_path, "selected", 1000, false).is_err());
        assert!(validate_peer(&info, &peer, &info.config_path, "restarted", 1000, true).is_err());
        assert!(validate_peer(
            &info,
            &peer,
            Path::new("/other/config.json"),
            "selected",
            1000,
            true
        )
        .is_err());
        let mut changed = info.clone();
        changed.mode = DaemonMode::System;
        assert!(validate_peer(&changed, &peer, &info.config_path, "selected", 1000, true).is_err());
        super::validate_peer(
            &changed,
            &peer,
            &info.config_path,
            "selected",
            1000,
            true,
            ServiceScope::System,
        )
        .unwrap();
        assert!(super::validate_peer(
            &changed,
            &peer,
            &info.config_path,
            "selected",
            1001,
            true,
            ServiceScope::System
        )
        .is_err());
        assert!(super::validate_peer(
            &changed,
            &peer,
            &info.config_path,
            "selected",
            1000,
            false,
            ServiceScope::System
        )
        .is_err());
        assert!(super::validate_peer(
            &info,
            &peer,
            &info.config_path,
            "selected",
            1000,
            true,
            ServiceScope::System
        )
        .is_err());
        changed = info.clone();
        changed
            .capabilities
            .retain(|capability| capability != SERVICE_WRITE_GATE);
        assert!(validate_peer(&changed, &peer, &info.config_path, "selected", 1000, true).is_err());
    }
}
