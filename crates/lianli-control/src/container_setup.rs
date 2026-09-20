use crate::account::Account;
use crate::container_deployment::Deployment;
use crate::reservation::{HardwareReservation, ServiceOperationLock};
use crate::service_operation::Backend;
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceScope, ServiceSelection};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LIMIT: usize = 64 * 1024;

fn write_file(path: &Path, contents: &str, previous: &[String], owner: u32) -> Result<()> {
    let boundary = if owner == 0 {
        PathBuf::from("/etc")
    } else {
        Account::user(owner)?.home
    };
    write_file_in(path, contents, previous, owner, &boundary)
}

fn write_file_in(
    path: &Path,
    contents: &str,
    previous: &[String],
    owner: u32,
    boundary: &Path,
) -> Result<()> {
    ensure!(contents.len() <= LIMIT, "Setup file exceeds 64 KiB");
    let parent = path.parent().context("Setup file has no directory")?;
    let directory = open_directory(parent, boundary, owner)?;
    let pinned = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let name = path.file_name().context("Setup file has no filename")?;
    let target = pinned.join(name);
    let existing = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&target)
    {
        Ok(file) => {
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file()
                    && metadata.uid() == owner
                    && metadata.mode() & 0o022 == 0
                    && metadata.nlink() == 1
                    && metadata.len() <= LIMIT as u64,
                "Existing setup file has unsafe ownership, permissions or size"
            );
            let mut text = String::new();
            file.take(LIMIT as u64 + 1).read_to_string(&mut text)?;
            ensure!(text.len() <= LIMIT, "Existing setup file exceeds 64 KiB");
            Some(text)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if existing.as_deref() == Some(contents) {
        return Ok(());
    }
    if let Some(existing) = existing {
        ensure!(
            previous.contains(&existing),
            "A custom support file needs review before replacement: {}",
            path.display()
        );
        let mut backup = tempfile::Builder::new()
            .prefix(".lianli-before-setup-")
            .tempfile_in(&pinned)?;
        backup.write_all(existing.as_bytes())?;
        backup.as_file().sync_all()?;
        backup.keep()?;
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&pinned)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(target)?;
    directory.sync_all()?;
    Ok(())
}

fn open_directory(path: &Path, boundary: &Path, owner: u32) -> Result<File> {
    let relative = path
        .strip_prefix(boundary)
        .context("Setup path left its expected directory")?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(boundary)?;
    let verify = |file: &File| -> Result<()> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == owner && metadata.mode() & 0o022 == 0,
            "Setup directory has unsafe ownership or permissions"
        );
        Ok(())
    };
    verify(&directory)?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("Invalid setup directory component")
        };
        let name = std::ffi::CString::new(name.as_encoded_bytes())?;
        let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) };
        if created != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        } else {
            directory.sync_all()?;
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
        verify(&directory)?;
    }
    Ok(directory)
}

fn initialize_locks() -> Result<()> {
    let metadata = fs::symlink_metadata("/run")?;
    ensure!(
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "Host runtime has unsafe permissions"
    );
    for path in [
        lianli_shared::installation::DAEMON_LOCK_PATH,
        lianli_shared::installation::SERVICE_OPERATION_LOCK_PATH,
    ] {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o666)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => {
                file.set_permissions(fs::Permissions::from_mode(0o666))?;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)?,
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.nlink() == 1
                && metadata.mode() & 0o777 == 0o666,
            "Repair the shared host lock ownership and permissions before setup"
        );
    }
    Ok(())
}

fn setup_selection(
    selection: Option<ServiceSelection>,
    owner_uid: u32,
    startup: crate::service_startup::Policy,
    system_missing: bool,
    managed: bool,
) -> Result<Option<ServiceSelection>> {
    use crate::switch_journal::Startup;
    if selection.is_none_or(|selection| selection.uid == owner_uid) {
        return Ok(selection);
    }
    ensure!(
        selection.is_some_and(|selection| selection.scope == ServiceScope::System)
            && system_missing
            && !managed
            && startup.user == Startup::Disabled
            && startup.system == Startup::Disabled,
        "The host selects another service account. Stop the previous daemon cleanly and use the host control helper to select user mode for UID {owner_uid} before retrying setup"
    );
    Ok(Some(ServiceSelection {
        scope: ServiceScope::User,
        uid: owner_uid,
    }))
}

pub fn install(deployment: &Deployment) -> Result<()> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Install managed services on the host"
    );
    let account = Account::authorized_caller()?;
    deployment.verify_owner(&account)?;
    let helper = crate::host_helper::installed()?;
    let helper = if fs::canonicalize(crate::host_helper::PACKAGED).ok().as_ref() == Some(&helper) {
        crate::host_helper::PACKAGED
    } else {
        crate::host_helper::STANDALONE
    };
    initialize_locks()?;
    let operation = ServiceOperationLock::acquire(&InstallationContext::Native)?;
    ensure!(
        crate::switch_journal::Journal::load(&operation)?.is_none(),
        "Recover the pending switch before setting up services"
    );
    let mut backend = crate::authorized_service::Authorized::new(&account, &operation)?;
    let report = backend.inspect()?;
    let startup = crate::service_startup::Policy::for_setup(&report)?;
    startup.validate()?;
    let selection = crate::service_selection::inspect(&InstallationContext::Native)?;
    for scope in [ServiceScope::User, ServiceScope::System] {
        ensure!(
            crate::native_switch::idle(crate::native_switch::unit(&report, scope)?),
            "Stop both hardware services before installing managed wrappers"
        );
    }
    let owner = crate::native_switch::ownership(&report)?;
    let system = crate::native_switch::unit(&report, ServiceScope::System)?;
    ensure!(
        system.load_state == "not-found"
            || system.distrobox_name.is_some()
            || matches!(
                system.unit_file_state.as_str(),
                "disabled" | "masked" | "masked-runtime" | ""
            ),
        "Disable native system-service startup before installing a boxed system wrapper"
    );
    ensure!(
        owner.owner_pid.is_none(),
        "Stop the current hardware daemon before setup"
    );
    let hardware = HardwareReservation::acquire(&InstallationContext::Native, &owner.identity)?;
    let previous = crate::container_deployment::load()?;
    if let Some(previous) = &previous {
        previous.verify_owner(&account)?;
    }
    let selected = setup_selection(
        selection,
        account.uid,
        startup,
        system.load_state == "not-found",
        previous.is_some(),
    )?;
    if selected != selection {
        // Publish before installing wrappers so interrupted setup can be retried safely.
        crate::service_selection::replace(selected, &operation, &hardware)?;
    }
    let mut old_system = Vec::new();
    if let Some(previous) = &previous {
        old_system.push(previous.unit(ServiceScope::System)?);
    }
    write_file(
        Path::new("/etc/systemd/system/lianli-daemon-system.service"),
        &deployment.unit(ServiceScope::System)?,
        &old_system,
        0,
    )?;
    let recovery = include_str!("../../../packaging/systemd/lianli-control-recovery.service");
    write_file(
        Path::new("/etc/systemd/system/lianli-control-recovery.service"),
        &recovery.replace(crate::host_helper::PACKAGED, helper),
        &[
            recovery.into(),
            recovery.replace(crate::host_helper::PACKAGED, crate::host_helper::STANDALONE),
        ],
        0,
    )?;
    write_file(
        Path::new("/etc/tmpfiles.d/lianli.conf"),
        include_str!("../../../packaging/tmpfiles.d/lianli.conf"),
        &[],
        0,
    )?;
    write_file(
        Path::new("/etc/polkit-1/rules.d/49-lianli-recovery.rules"),
        include_str!("../../../packaging/polkit/49-lianli-recovery.rules"),
        &[],
        0,
    )?;
    let desktop =
        include_str!("../../../packaging/desktop/com.sgtaziz.lianlilinux.recovery.desktop");
    write_file(
        Path::new("/etc/xdg/autostart/com.sgtaziz.lianlilinux.recovery.desktop"),
        &desktop.replace(crate::host_helper::PACKAGED, helper),
        &[
            desktop.into(),
            desktop.replace(crate::host_helper::PACKAGED, crate::host_helper::STANDALONE),
        ],
        0,
    )?;
    let text = serde_json::to_string(deployment)?;
    let previous = serde_json::to_string(&previous)?;
    let output = crate::command::run(
        account.control_command(&[
            "install-container-user-units".as_ref(),
            "--deployment".as_ref(),
            text.as_ref(),
            "--previous".as_ref(),
            previous.as_ref(),
        ])?,
        Duration::from_secs(30),
    )?;
    ensure!(
        output.status.success(),
        "User wrapper installation failed: {}",
        output.stderr.trim()
    );
    let output = crate::services::Route::Native.output(
        "/usr/bin/systemctl",
        &[
            "--system",
            "--no-reload",
            "add-wants",
            "multi-user.target",
            "lianli-control-recovery.service",
        ],
    )?;
    ensure!(
        output.status.success(),
        "Boot recovery setup failed: {}",
        output.stderr.trim()
    );
    let output = crate::services::Route::Native
        .output("/usr/bin/systemctl", &["--system", "daemon-reload"])?;
    ensure!(
        output.status.success(),
        "System unit reload failed: {}",
        output.stderr.trim()
    );
    operation.verify()?;
    hardware.verify()?;
    crate::container_deployment::install(deployment, &account, &operation, &hardware)
}

pub fn install_user(deployment: &Deployment, previous: Option<&Deployment>) -> Result<()> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Install user wrappers on the host"
    );
    let account = Account::user(unsafe { libc::geteuid() })?;
    deployment.verify_owner(&account)?;
    let launch = &deployment.route.launch;
    let mut old = vec![crate::distrobox_unit::generate(
        &launch.name,
        &launch.host_enter,
        &launch.binaries,
    )?];
    if let Some(previous) = previous {
        previous.verify_owner(&account)?;
        old.push(previous.unit(ServiceScope::User)?);
    }
    let directory = account.home.join(".config/systemd/user");
    write_file(
        &directory.join(ServiceScope::User.unit()),
        &deployment.unit(ServiceScope::User)?,
        &old,
        account.uid,
    )?;
    let session = crate::distrobox_unit::generate_session(
        &launch.name,
        &launch.host_enter,
        &launch.binaries,
    )?;
    let old_session = previous
        .map(|previous| {
            let launch = &previous.route.launch;
            crate::distrobox_unit::generate_session(
                &launch.name,
                &launch.host_enter,
                &launch.binaries,
            )
        })
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    write_file(
        &directory.join("lianli-session.service"),
        &session,
        &old_session,
        account.uid,
    )?;
    let output = crate::services::Route::Native
        .output("/usr/bin/systemctl", &["--user", "daemon-reload"])?;
    ensure!(
        output.status.success(),
        "User unit reload failed: {}",
        output.stderr.trim()
    );
    let output = crate::services::Route::Native.output(
        "/usr/bin/systemctl",
        &["--user", "enable", "lianli-session.service"],
    )?;
    ensure!(
        output.status.success(),
        "Desktop capture startup could not be enabled: {}",
        output.stderr.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_recovers_only_an_uninstalled_system_selection() {
        use crate::service_startup::Policy;
        use crate::switch_journal::Startup;
        let disabled = Policy {
            user: Startup::Disabled,
            system: Startup::Disabled,
        };
        let old = Some(ServiceSelection {
            scope: ServiceScope::System,
            uid: 930,
        });
        let user = Some(ServiceSelection {
            scope: ServiceScope::User,
            uid: 1000,
        });
        assert_eq!(
            setup_selection(old, 1000, disabled, true, false).unwrap(),
            user
        );
        for scope in [ServiceScope::User, ServiceScope::System] {
            let owned = Some(ServiceSelection { scope, uid: 1000 });
            assert_eq!(
                setup_selection(owned, 1000, disabled, false, true).unwrap(),
                owned
            );
        }
        assert_eq!(
            setup_selection(None, 1000, disabled, true, false).unwrap(),
            None
        );
        assert!(setup_selection(old, 1000, disabled, false, false).is_err());
        assert!(setup_selection(old, 1000, disabled, true, true).is_err());
        let foreign_user = Some(ServiceSelection {
            scope: ServiceScope::User,
            uid: 1001,
        });
        assert!(setup_selection(foreign_user, 1000, disabled, true, false).is_err());
        for enabled in [Startup::Enabled, Startup::Runtime] {
            for startup in [
                Policy {
                    user: enabled,
                    ..disabled
                },
                Policy {
                    system: enabled,
                    ..disabled
                },
            ] {
                assert!(setup_selection(old, 1000, startup, true, false).is_err());
            }
        }
    }

    #[test]
    fn setup_is_idempotent_and_preserves_recognized_replacements() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("systemd/user/fixture.service");
        let owner = unsafe { libc::geteuid() };
        write_file_in(&path, "old", &[], owner, root.path()).unwrap();
        write_file_in(&path, "old", &[], owner, root.path()).unwrap();
        write_file_in(&path, "new", &["old".into()], owner, root.path()).unwrap();
        write_file_in(&path, "new", &[], owner, root.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let files = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 2);
        let backup = files
            .iter()
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".lianli-before-setup-")
            })
            .unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), "old");
        assert_eq!(fs::metadata(backup).unwrap().mode() & 0o077, 0);
        assert!(write_file_in(&path, "another", &[], owner, root.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn setup_rejects_symlinked_files_and_directories_without_changing_their_targets() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let owner = unsafe { libc::geteuid() };
        let target = other.path().join("target");
        fs::write(&target, "keep").unwrap();
        symlink(&target, root.path().join("file")).unwrap();
        assert!(write_file_in(
            &root.path().join("file"),
            "replace",
            &["keep".into()],
            owner,
            root.path()
        )
        .is_err());
        symlink(other.path(), root.path().join("directory")).unwrap();
        assert!(write_file_in(
            &root.path().join("directory/new"),
            "replace",
            &[],
            owner,
            root.path()
        )
        .is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
        assert!(!other.path().join("new").exists());
        assert!(open_directory(&root.path().join("../escape"), root.path(), owner).is_err());
    }

    #[test]
    fn setup_rejects_writable_directories_and_hardlinked_files() {
        let root = tempfile::tempdir().unwrap();
        let owner = unsafe { libc::geteuid() };
        let file = root.path().join("unit");
        write_file_in(&file, "old", &[], owner, root.path()).unwrap();
        fs::hard_link(&file, root.path().join("alias")).unwrap();
        assert!(write_file_in(&file, "new", &["old".into()], owner, root.path()).is_err());
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            write_file_in(&root.path().join("another"), "new", &[], owner, root.path()).is_err()
        );
        assert_eq!(fs::read_to_string(file).unwrap(), "old");
    }
}
