use crate::account::Account;
use crate::container_destination::Route;
use crate::reservation::{HardwareReservation, ServiceOperationLock};
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::ServiceScope;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const DIRECTORY: &str = "/etc/lianli-control";
const NAME: &str = "distrobox.json";
const LIMIT: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub version: u32,
    pub owner_uid: u32,
    pub owner_name: String,
    pub route: Route,
}

impl Deployment {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "Unsupported Distrobox deployment version"
        );
        ensure!(
            self.owner_uid != 0
                && !self.owner_name.is_empty()
                && self.owner_name.len() <= 256
                && !self.owner_name.chars().any(char::is_control),
            "Invalid Distrobox deployment owner"
        );
        self.route.validate()
    }

    pub fn verify_owner(&self, account: &Account) -> Result<()> {
        self.validate()?;
        ensure!(
            account.container.is_none()
                && account.uid == self.owner_uid
                && account.name == self.owner_name,
            "The Distrobox deployment belongs to another host account"
        );
        Ok(())
    }

    pub fn switch_accounts(&self, host: &Account) -> Result<(Account, Account)> {
        self.verify_installed(host)?;
        let discover = |scope| {
            let (config, working) = self.route.paths(scope);
            crate::container_destination::Execution::discover(
                host,
                self.route.launch.clone(),
                scope,
                config,
                working,
                true,
            )
            .map(|(execution, _)| execution)
        };
        let user = discover(ServiceScope::User)?;
        let system = discover(ServiceScope::System)?;
        user.ensure_separate(&system)?;
        Ok((
            host.clone().with_container(user)?,
            host.clone().with_container(system)?,
        ))
    }

    pub fn verify_execution(
        &self,
        execution: &crate::container_destination::Execution,
    ) -> Result<()> {
        self.validate()?;
        let identity = &execution.destination;
        identity.validate()?;
        let (config, working) = self.route.paths(identity.scope);
        ensure!(
            execution.launch == self.route.launch
                && identity.name == self.route.launch.name
                && identity.uid == self.owner_uid
                && identity.config_path == config
                && identity.working_directory == working,
            "The container execution differs from the protected deployment"
        );
        Ok(())
    }

    pub fn unit(&self, scope: ServiceScope) -> Result<String> {
        self.validate()?;
        crate::distrobox_unit::generate_managed(&self.route, scope, self.owner_uid)
    }

    pub fn verify_unit(&self, scope: ServiceScope, contents: &str) -> Result<()> {
        ensure!(
            contents == self.unit(scope)?,
            "The installed Distrobox wrapper differs from its deployment record"
        );
        Ok(())
    }

    pub fn inspect_installed(&self) -> Result<()> {
        host_context()?;
        let account = Account::user(unsafe { libc::geteuid() })?;
        self.verify_owner(&account)?;
        for scope in [ServiceScope::User, ServiceScope::System] {
            let path = match scope {
                ServiceScope::User => account.home.join(".config/systemd/user").join(scope.unit()),
                ServiceScope::System => Path::new("/etc/systemd/system").join(scope.unit()),
            };
            self.inspect_unit(scope, &path)?;
        }
        Ok(())
    }

    pub(crate) fn inspect_system(&self) -> Result<()> {
        host_context()?;
        self.validate()?;
        self.inspect_unit(
            ServiceScope::System,
            &Path::new("/etc/systemd/system").join(ServiceScope::System.unit()),
        )
    }

    fn inspect_unit(&self, scope: ServiceScope, path: &Path) -> Result<()> {
        let output = crate::services::Route::Native.output(
            "/usr/bin/systemctl",
            &[
                scope.argument(),
                "--no-pager",
                "--no-ask-password",
                "show",
                "--property=LoadState,FragmentPath,DropInPaths,NeedDaemonReload,User,Transient",
                scope.unit(),
            ],
        )?;
        ensure!(
            output.status.success(),
            "Cannot inspect installed container wrapper: {}",
            output.stderr.trim()
        );
        self.verify_properties(scope, path, &output.stdout)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        let owner = if scope == ServiceScope::System {
            0
        } else {
            self.owner_uid
        };
        ensure!(
            metadata.is_file()
                && metadata.uid() == owner
                && metadata.mode() & 0o022 == 0
                && metadata.nlink() == 1
                && metadata.len() <= LIMIT as u64,
            "Installed container wrapper has unsafe ownership, permissions or size"
        );
        let mut contents = String::new();
        file.take(LIMIT as u64 + 1).read_to_string(&mut contents)?;
        ensure!(
            contents.len() <= LIMIT,
            "Installed container wrapper exceeds 32 KiB"
        );
        self.verify_unit(scope, &contents)?;
        Ok(())
    }

    fn verify_properties(&self, scope: ServiceScope, path: &Path, text: &str) -> Result<()> {
        let properties: std::collections::HashMap<_, _> = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        ensure!(
            properties.get("LoadState") == Some(&"loaded")
                && properties.get("NeedDaemonReload") == Some(&"no")
                && properties.get("Transient") == Some(&"no")
                && properties.get("DropInPaths") == Some(&"")
                && properties.get("FragmentPath").map(Path::new) == Some(path),
            "Install the managed wrapper without overrides and reload the service manager"
        );
        let owner = if scope == ServiceScope::System {
            self.owner_uid.to_string()
        } else {
            String::new()
        };
        ensure!(
            properties.get("User").copied() == Some(owner.as_str()),
            "The loaded wrapper uses another service account"
        );
        Ok(())
    }

    pub fn verify_installed(&self, account: &Account) -> Result<()> {
        self.verify_owner(account)?;
        host_context()?;
        let text = serde_json::to_string(self)?;
        ensure!(text.len() <= LIMIT, "Deployment record exceeds 32 KiB");
        let output = crate::command::run(
            account.control_command(&[
                std::ffi::OsStr::new("inspect-container-deployment"),
                std::ffi::OsStr::new("--deployment"),
                std::ffi::OsStr::new(&text),
            ])?,
            std::time::Duration::from_secs(60),
        )?;
        ensure!(
            output.status.success(),
            "Container wrapper verification failed: {}",
            output.stderr.trim()
        );
        Ok(())
    }
}

fn host_context() -> Result<()> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Inspect deployment records through the native host helper"
    );
    let metadata = fs::symlink_metadata("/etc")?;
    ensure!(
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "The host configuration directory has unsafe ownership or permissions"
    );
    Ok(())
}

pub fn load() -> Result<Option<Deployment>> {
    host_context()?;
    let Some(store) = Store::open(Path::new(DIRECTORY), 0, false)? else {
        return Ok(None);
    };
    store.read()
}

pub fn install(
    deployment: &Deployment,
    account: &Account,
    operation: &ServiceOperationLock,
    hardware: &HardwareReservation,
) -> Result<()> {
    host_context()?;
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "Deployment installation requires host authorization"
    );
    deployment.verify_owner(account)?;
    ensure!(
        &Account::user(account.uid)? == account,
        "The host account changed during setup"
    );
    operation.verify()?;
    hardware.verify()?;
    ensure!(
        crate::switch_journal::Journal::load(operation)?.is_none(),
        "Recover the pending switch before changing deployment"
    );
    deployment.verify_installed(account)?;
    operation.verify()?;
    hardware.verify()?;
    Store::open(Path::new(DIRECTORY), 0, true)?
        .context("Deployment directory is unavailable")?
        .write(deployment)?;
    operation.verify()?;
    hardware.verify()
}

struct Store {
    directory: File,
    owner: u32,
}

impl Store {
    fn open(path: &Path, owner: u32, create: bool) -> Result<Option<Self>> {
        if create {
            match fs::DirBuilder::new().mode(0o755).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let directory = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = directory.metadata()?;
        ensure!(
            metadata.uid() == owner && metadata.mode() & 0o022 == 0,
            "Deployment directory has unsafe ownership or permissions"
        );
        Ok(Some(Self { directory, owner }))
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }

    fn read(&self) -> Result<Option<Deployment>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path().join(NAME))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == self.owner
                && metadata.mode() & 0o022 == 0
                && metadata.nlink() == 1
                && metadata.len() <= LIMIT as u64,
            "Deployment record has unsafe ownership, permissions or size"
        );
        let mut bytes = Vec::new();
        file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= LIMIT, "Deployment record exceeds 32 KiB");
        let deployment: Deployment = serde_json::from_slice(&bytes)?;
        deployment.validate()?;
        Ok(Some(deployment))
    }

    fn write(&self, deployment: &Deployment) -> Result<()> {
        deployment.validate()?;
        self.read()?;
        let bytes = serde_json::to_vec_pretty(deployment)?;
        ensure!(bytes.len() <= LIMIT, "Deployment record exceeds 32 KiB");
        let mut temporary = tempfile::NamedTempFile::new_in(self.path())?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(self.path().join(NAME))?;
        self.directory.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the hardware-free local release VM with managed Distrobox units"]
    fn installed_system_wrapper_can_be_verified_by_root() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        assert_eq!(
            fs::read_to_string("/proc/sys/kernel/hostname")
                .unwrap()
                .trim(),
            "lianli-release-test"
        );
        assert!(!Path::new("/dev/bus/usb").exists() && !Path::new("/dev/dri").exists());
        let deployment = load().unwrap().unwrap();
        assert_eq!(deployment.owner_uid, 1000);
        assert_eq!(deployment.route.launch.name, "lianli-release-box");
        deployment.inspect_system().unwrap();
    }

    fn deployment() -> Deployment {
        Deployment {
            version: 1,
            owner_uid: 1000,
            owner_name: "fixture".into(),
            route: Route {
                launch: crate::container_destination::Launch {
                    name: "fixture-box".into(),
                    host_enter: "/usr/bin/distrobox-enter".into(),
                    binaries: "/usr/bin".into(),
                },
                user_config: "/home/fixture/user/config.json".into(),
                system_config: "/home/fixture/system/config.json".into(),
                user_working_directory: "/home/fixture".into(),
                system_working_directory: "/home/fixture".into(),
            },
        }
    }

    #[test]
    fn deployment_roundtrip_preserves_route_and_replaces_atomically() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("deployment");
        let owner = unsafe { libc::geteuid() };
        assert!(Store::open(&path, owner, false).unwrap().is_none());
        let store = Store::open(&path, owner, true).unwrap().unwrap();
        assert!(store.read().unwrap().is_none());
        let original = deployment();
        store.write(&original).unwrap();
        let old = File::open(path.join(NAME)).unwrap();
        let mut replacement = original.clone();
        replacement.route.launch.binaries = "/opt/lianli".into();
        store.write(&replacement).unwrap();
        assert_eq!(store.read().unwrap(), Some(replacement));
        assert_eq!(
            serde_json::from_reader::<_, Deployment>(old).unwrap(),
            original
        );
        assert_eq!(fs::read_dir(path).unwrap().count(), 1);
    }

    #[test]
    fn unsafe_or_malformed_records_cannot_be_read_or_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let owner = unsafe { libc::geteuid() };
        let store = Store::open(root.path(), owner, false).unwrap().unwrap();
        let path = root.path().join(NAME);
        let original = deployment();
        store.write(&original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(store.read().is_err());
        assert!(store.write(&original).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(store.read().is_err());
        fs::remove_file(root.path().join("alias")).unwrap();
        fs::write(&path, b"{broken").unwrap();
        assert!(store.write(&original).is_err());
        fs::remove_file(&path).unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(store.read().is_err());
        assert!(store.write(&original).is_err());
        assert_eq!(fs::read(target).unwrap(), b"unchanged");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Store::open(root.path(), owner, false).is_err());
    }

    #[test]
    fn execution_must_match_protected_owner_launcher_and_mode_paths() {
        use crate::container_destination::{Execution, Identity};
        use lianli_shared::daemon::FileIdentity;
        let record = deployment();
        let mut execution = Execution {
            launch: record.route.launch.clone(),
            destination: Identity {
                name: record.route.launch.name.clone(),
                scope: ServiceScope::User,
                uid: record.owner_uid,
                gid: 1000,
                groups_fingerprint: "a".repeat(64),
                mount_namespace: FileIdentity {
                    device: "1".into(),
                    inode: "2".into(),
                },
                state_directory: FileIdentity {
                    device: "1".into(),
                    inode: "3".into(),
                },
                config_path: record.route.user_config.clone(),
                working_directory: record.route.user_working_directory.clone(),
            },
        };
        record.verify_execution(&execution).unwrap();
        execution.destination.scope = ServiceScope::System;
        assert!(record.verify_execution(&execution).is_err());
        execution.destination.config_path = record.route.system_config.clone();
        record.verify_execution(&execution).unwrap();
        execution.destination.uid += 1;
        assert!(record.verify_execution(&execution).is_err());
        execution.destination.uid = record.owner_uid;
        execution.launch.name = "other-box".into();
        assert!(record.verify_execution(&execution).is_err());
    }

    #[test]
    fn loaded_wrapper_checks_reject_overrides_stale_units_and_other_owners() {
        let record = deployment();
        let path = Path::new("/etc/systemd/system/lianli-daemon-system.service");
        let properties = format!("LoadState=loaded\nNeedDaemonReload=no\nTransient=no\nDropInPaths=\nFragmentPath={}\nUser=1000\n", path.display());
        record
            .verify_properties(ServiceScope::System, path, &properties)
            .unwrap();
        for (old, new) in [
            ("LoadState=loaded", "LoadState=not-found"),
            ("NeedDaemonReload=no", "NeedDaemonReload=yes"),
            ("Transient=no", "Transient=yes"),
            (
                "DropInPaths=",
                "DropInPaths=/run/systemd/system/override.conf",
            ),
            ("FragmentPath=/etc", "FragmentPath=/run"),
            ("User=1000", "User=0"),
        ] {
            assert!(
                record
                    .verify_properties(ServiceScope::System, path, &properties.replace(old, new))
                    .is_err(),
                "{old}"
            );
        }
        assert!(record
            .verify_properties(ServiceScope::System, path, "")
            .is_err());
        record
            .verify_properties(
                ServiceScope::User,
                path,
                &properties.replace("User=1000", "User="),
            )
            .unwrap();
    }

    #[test]
    fn wrapper_verification_rejects_other_modes_paths_and_extra_commands() {
        let record = deployment();
        let unit = record.unit(ServiceScope::System).unwrap();
        record.verify_unit(ServiceScope::System, &unit).unwrap();
        assert!(record.verify_unit(ServiceScope::User, &unit).is_err());
        assert!(record
            .verify_unit(
                ServiceScope::System,
                &unit.replace("fixture-box", "other-box")
            )
            .is_err());
        assert!(record
            .verify_unit(
                ServiceScope::System,
                &format!("{unit}\n[Service]\nExecStartPost=/bin/true\n")
            )
            .is_err());
        let mut bad = record;
        bad.owner_uid = 0;
        assert!(bad.validate().is_err());
        bad.owner_uid = 1000;
        bad.version = 2;
        assert!(bad.validate().is_err());
    }
}
