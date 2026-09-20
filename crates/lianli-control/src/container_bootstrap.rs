use crate::container_deployment::Deployment;
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

const SCRIPT: &str = include_str!("../../../packaging/distrobox/bootstrap-host.sh");
const LIMIT: u64 = 128 * 1024 * 1024;

pub(crate) fn deployment_for_action() -> Result<Option<Deployment>> {
    let InstallationContext::Distrobox { name } = InstallationContext::detect() else {
        anyhow::bail!("Set up host support from the selected Distrobox");
    };
    match crate::container_change::deployment() {
        Ok(deployment) => Ok(deployment),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied) =>
        {
            let binaries = std::env::current_exe()?
                .parent()
                .context("Application directory is unavailable")?
                .to_path_buf();
            authorize(&binaries, "repair", &name)?;
            crate::container_change::deployment()
        }
        Err(error) => Err(error),
    }
}

pub fn proposal(user_config: Option<PathBuf>) -> Result<Deployment> {
    let InstallationContext::Distrobox { name } = InstallationContext::detect() else {
        anyhow::bail!("Set up host support from the selected Distrobox");
    };
    if let Some(deployment) = deployment_for_action()? {
        return Ok(deployment);
    }
    let account = crate::account::Account::user(unsafe { libc::geteuid() })?;
    let route =
        crate::services::Route::detect(&InstallationContext::Distrobox { name: name.clone() })?;
    let uid = route.output("/usr/bin/id", &["-u"])?;
    let owner = route.output("/usr/bin/id", &["-un"])?;
    ensure!(
        uid.status.success()
            && uid.stdout.trim() == account.uid.to_string()
            && owner.status.success(),
        "The host and box must use the same account for managed services"
    );
    let user_config = user_config.unwrap_or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| account.home.join(".config"))
            .join("lianli/config.json")
    });
    let deployment = Deployment {
        version: 1,
        owner_uid: account.uid,
        owner_name: owner.stdout.trim().into(),
        route: crate::container_destination::Route {
            launch: crate::container_destination::Launch {
                name,
                host_enter: "/usr/bin/distrobox-enter".into(),
                binaries: std::env::current_exe()?
                    .parent()
                    .context("Application directory is unavailable")?
                    .into(),
            },
            user_config,
            system_config: account.home.join(".local/share/lianli-system/config.json"),
            user_working_directory: account.home.clone(),
            system_working_directory: account.home,
        },
    };
    deployment.validate()?;
    Ok(deployment)
}

fn stage(source: &std::path::Path, target: &std::path::Path) -> Result<String> {
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(source)?;
    let metadata = source.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= LIMIT,
        "The bundled host helper is unavailable or exceeds 128 MiB"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(target)?;
    let mut digest = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if copied == 0 {
            ensure!(
                buffer[..count].starts_with(b"\x7fELF"),
                "The bundled host helper is not an ELF executable"
            );
        }
        copied += count as u64;
        ensure!(copied <= LIMIT, "The bundled host helper exceeds 128 MiB");
        output.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
    }
    ensure!(copied >= 4, "The bundled host helper is empty");
    output.sync_all()?;
    Ok(format!("{:x}", digest.finalize()))
}

pub fn install(deployment: &Deployment) -> Result<()> {
    let InstallationContext::Distrobox { name } = InstallationContext::detect() else {
        anyhow::bail!("Bootstrap host support from the selected Distrobox");
    };
    deployment.validate()?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        uid != 0 && deployment.owner_uid == uid && deployment.route.launch.name == name,
        "Host setup must use this box and its original account"
    );
    authorize(
        &deployment.route.launch.binaries,
        "install",
        &serde_json::to_string(deployment)?,
    )
}

fn authorize(binaries: &std::path::Path, action: &str, text: &str) -> Result<()> {
    let InstallationContext::Distrobox { name } = InstallationContext::detect() else {
        anyhow::bail!("Bootstrap host support from the selected Distrobox");
    };
    let uid = unsafe { libc::geteuid() };
    ensure!(uid != 0, "Host setup requires the original box owner");
    ensure!(text.len() <= 32 * 1024, "Host setup request exceeds 32 KiB");
    let route = crate::services::Route::detect(&InstallationContext::Distrobox { name })?;
    let runtime = PathBuf::from(format!("/run/host/run/user/{uid}"));
    let metadata = fs::symlink_metadata(&runtime)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "The shared host user runtime is unavailable or not private"
    );
    let directory = tempfile::Builder::new()
        .prefix("lianli-host-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(runtime)?;
    let host_directory = PathBuf::from(format!("/run/user/{uid}")).join(
        directory
            .path()
            .file_name()
            .context("Staging directory has no name")?,
    );
    let host_directory_text = host_directory
        .to_str()
        .context("Host staging path is not UTF-8")?;
    let host = route.output(
        "/usr/bin/stat",
        &["-Lc", "%d:%i:%u:%a", "--", host_directory_text],
    )?;
    let metadata = directory.path().metadata()?;
    ensure!(
        host.status.success()
            && host.stdout.trim() == format!("{}:{}:{uid}:700", metadata.dev(), metadata.ino()),
        "The host and box do not share the same private staging directory"
    );
    let digest = stage(
        &binaries.join("lianli-control"),
        &directory.path().join("lianli-control"),
    )?;
    let host_binary = host_directory.join("lianli-control");
    let host_binary = host_binary
        .to_str()
        .context("Host helper path is not UTF-8")?;
    let version = route.bounded_output(host_binary, &["--version"], Duration::from_secs(10))?;
    ensure!(version.status.success() && version.stdout.trim() == format!("lianli-control {}", env!("CARGO_PKG_VERSION")),
        "The host cannot run the bundled control helper. Install a host-compatible helper before setting up services. {}", version.stderr.trim());
    let output = route
        .bounded_output(
            "/usr/bin/pkexec",
            &[
                "--disable-internal-agent",
                "/usr/bin/sh",
                "-c",
                SCRIPT,
                "lianli-host-setup",
                &uid.to_string(),
                &digest,
                host_binary,
                text,
                action,
            ],
            Duration::from_secs(300),
        )
        .context("Host setup was not confirmed. Recheck installation before retrying")?;
    ensure!(
        output.status.success(),
        "Host setup failed: {}",
        output.stderr.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_helper_preserves_bytes_digest_and_private_permissions() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("lianli-control");
        let bytes = b"\x7fELFtest fixture bytes";
        fs::write(&source, bytes).unwrap();
        let digest = stage(&source, &target).unwrap();
        assert_eq!(digest, format!("{:x}", Sha256::digest(bytes)));
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(target.metadata().unwrap().mode() & 0o777, 0o700);
        assert!(stage(&source, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), bytes);
    }

    #[test]
    fn staging_rejects_scripts_symlinks_and_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(stage(&source, &root.path().join("script")).is_err());
        std::os::unix::fs::symlink(&source, root.path().join("link")).unwrap();
        assert!(stage(&root.path().join("link"), &root.path().join("linked")).is_err());
        OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(LIMIT + 1)
            .unwrap();
        assert!(stage(&source, &root.path().join("oversized")).is_err());
        assert!(!root.path().join("oversized").exists());
    }
}
