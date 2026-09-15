use crate::account::Account;
use crate::destination::Destination;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::FileIdentity;
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::ServiceScope;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub name: String,
    pub host_enter: PathBuf,
    pub binaries: PathBuf,
}

impl Launch {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            crate::distrobox_unit::valid_name(&self.name),
            "Invalid Distrobox destination name"
        );
        absolute(&self.host_enter)?;
        absolute(&self.binaries)?;
        ensure!(
            self.host_enter.file_name() == Some(OsStr::new("distrobox-enter")),
            "Select the host distrobox-enter executable"
        );
        Ok(())
    }

    fn arguments(&self, destination: Option<&Identity>, args: &[&OsStr]) -> Result<Vec<OsString>> {
        self.validate()?;
        let mut result = vec![
            "box-worker".into(),
            "--box".into(),
            self.name.clone().into(),
            "--distrobox-enter".into(),
            self.host_enter.clone().into_os_string(),
            "--binaries".into(),
            self.binaries.clone().into_os_string(),
        ];
        if let Some(destination) = destination {
            destination.validate()?;
            ensure!(
                destination.name == self.name,
                "Worker destination belongs to another box"
            );
            result.extend([
                "--destination".into(),
                serde_json::to_string(destination)?.into(),
            ]);
        }
        result.push("--".into());
        result.extend(args.iter().map(|value| value.to_os_string()));
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub scope: ServiceScope,
    pub uid: u32,
    pub gid: u32,
    pub groups_fingerprint: String,
    pub mount_namespace: FileIdentity,
    pub state_directory: FileIdentity,
    pub config_path: PathBuf,
    pub working_directory: PathBuf,
}

impl Identity {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            crate::distrobox_unit::valid_name(&self.name) && self.uid != 0,
            "Container state workers require a named box and unprivileged account"
        );
        validate_config(&self.config_path)?;
        absolute(&self.working_directory)?;
        ensure!(
            self.groups_fingerprint.len() == 64
                && self
                    .groups_fingerprint
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit()),
            "Invalid container credential fingerprint"
        );
        ensure!(
            [&self.mount_namespace, &self.state_directory]
                .iter()
                .all(|identity| identity.device.parse::<u64>().is_ok()
                    && identity.inode.parse::<u64>().is_ok()),
            "Invalid container filesystem identity"
        );
        Ok(())
    }

    pub(crate) fn matches(&self, destination: &Destination) -> bool {
        self.scope == destination.scope
            && self.uid == destination.uid
            && self.gid == destination.gid
            && self.groups_fingerprint == destination.groups_fingerprint
            && self.mount_namespace == destination.mount_namespace
            && destination.state_directory.as_ref() == Some(&self.state_directory)
            && self.config_path == destination.config_path
            && self.working_directory == destination.working_directory
    }

    pub(crate) fn verify(&self) -> Result<Account> {
        self.validate()?;
        let account = current_account(&self.name)?;
        let directory = std::fs::symlink_metadata(
            self.config_path
                .parent()
                .context("Configuration has no parent directory")?,
        )?;
        ensure!(
            directory.is_dir()
                && directory.uid() == self.uid
                && directory.mode() & 0o022 == 0
                && self.state_directory.device == directory.dev().to_string()
                && self.state_directory.inode == directory.ino().to_string(),
            "The container state directory changed after preflight"
        );
        ensure!(
            self.uid == account.uid
                && self.gid == account.gid
                && self.groups_fingerprint == account.group_fingerprint()
                && self.mount_namespace == namespace()?,
            "The container worker's credentials or filesystem namespace changed after preflight"
        );
        Ok(account)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Execution {
    pub launch: Launch,
    pub destination: Identity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub launch: Launch,
    pub user_config: PathBuf,
    pub system_config: PathBuf,
    pub user_working_directory: PathBuf,
    pub system_working_directory: PathBuf,
}

impl Route {
    pub fn validate(&self) -> Result<()> {
        self.launch.validate()?;
        validate_config(&self.user_config)?;
        validate_config(&self.system_config)?;
        absolute(&self.user_working_directory)?;
        absolute(&self.system_working_directory)?;
        ensure!(
            self.user_config.parent() != self.system_config.parent(),
            "Container service modes must use separate state directories"
        );
        Ok(())
    }

    pub fn paths(&self, scope: ServiceScope) -> (&Path, &Path) {
        match scope {
            ServiceScope::User => (&self.user_config, &self.user_working_directory),
            ServiceScope::System => (&self.system_config, &self.system_working_directory),
        }
    }

    pub fn recovery_accounts(&self, host: &Account) -> Result<(Account, Account)> {
        self.validate()?;
        let discover = |scope| {
            let (config, working) = self.paths(scope);
            Execution::for_recovery(host, self.launch.clone(), scope, config, working)
        };
        let user = discover(ServiceScope::User)?;
        let system = discover(ServiceScope::System)?;
        user.ensure_separate(&system)?;
        Ok((
            host.clone().with_container(user)?,
            host.clone().with_container(system)?,
        ))
    }
}

impl Execution {
    pub fn for_recovery(
        account: &Account,
        launch: Launch,
        scope: ServiceScope,
        config: &Path,
        working: &Path,
    ) -> Result<Self> {
        ensure!(
            InstallationContext::detect() == InstallationContext::Native
                && account.container.is_none(),
            "Inspect container recovery through its original host account"
        );
        validate_config(config)?;
        absolute(working)?;
        let args = launch.arguments(
            None,
            &[
                OsStr::new("inspect-container-identity"),
                OsStr::new("--box"),
                OsStr::new(&launch.name),
                OsStr::new("--scope"),
                OsStr::new(match scope {
                    ServiceScope::User => "user",
                    ServiceScope::System => "system",
                }),
                OsStr::new("--config"),
                config.as_os_str(),
                OsStr::new("--working-directory"),
                working.as_os_str(),
            ],
        )?;
        let args: Vec<_> = args.iter().map(OsString::as_os_str).collect();
        let output = crate::command::run(account.box_command(&args)?, Duration::from_secs(60))?;
        ensure!(
            output.status.success(),
            "Container recovery identity is unavailable: {}",
            output.stderr.trim()
        );
        let destination: Identity = serde_json::from_str(&output.stdout)?;
        destination.validate()?;
        ensure!(
            destination.uid == account.uid
                && destination.scope == scope
                && destination.name == launch.name
                && destination.config_path == config
                && destination.working_directory == working,
            "Container recovery identity differs from the requested destination"
        );
        Ok(Self {
            launch,
            destination,
        })
    }

    pub fn discover(
        account: &Account,
        launch: Launch,
        scope: ServiceScope,
        config: &Path,
        working: &Path,
        prepare: bool,
    ) -> Result<(Self, Destination)> {
        ensure!(
            InstallationContext::detect() == InstallationContext::Native,
            "Discover container destinations through the host coordinator"
        );
        ensure!(
            account.container.is_none(),
            "Discover container destinations from the host account"
        );
        validate_config(config)?;
        absolute(working)?;
        let scope_arg = match scope {
            ServiceScope::User => "user",
            ServiceScope::System => "system",
        };
        let mut arguments = vec![
            OsStr::new("inspect-container-destination"),
            OsStr::new("--box"),
            OsStr::new(&launch.name),
            OsStr::new("--scope"),
            OsStr::new(scope_arg),
            OsStr::new("--config"),
            config.as_os_str(),
            OsStr::new("--working-directory"),
            working.as_os_str(),
        ];
        if prepare {
            arguments.push(OsStr::new("--prepare-directory"));
        }
        let arguments = launch.arguments(None, &arguments)?;
        let arguments: Vec<_> = arguments.iter().map(OsString::as_os_str).collect();
        let output =
            crate::command::run(account.box_command(&arguments)?, Duration::from_secs(60))?;
        ensure!(
            output.status.success(),
            "Container destination preflight failed: {}",
            output.stderr.trim()
        );
        let destination: Destination = serde_json::from_str(&output.stdout)
            .context("Invalid container destination response")?;
        ensure!(
            destination.uid == account.uid
                && destination.scope == scope
                && destination.config_path == config
                && destination.working_directory == working
                && destination.assets.uid == account.uid,
            "Container destination differs from the requested account, mode or paths"
        );
        let identity = Identity {
            name: launch.name.clone(),
            scope,
            uid: destination.uid,
            gid: destination.gid,
            groups_fingerprint: destination.groups_fingerprint.clone(),
            mount_namespace: destination.mount_namespace.clone(),
            state_directory: destination
                .state_directory
                .clone()
                .context("Container preflight did not report its state directory identity")?,
            config_path: destination.config_path.clone(),
            working_directory: destination.working_directory.clone(),
        };
        identity.validate()?;
        Ok((
            Self {
                launch,
                destination: identity,
            },
            destination,
        ))
    }

    pub(crate) fn command(&self, account: &Account, args: &[&OsStr]) -> Result<Command> {
        ensure!(
            InstallationContext::detect() == InstallationContext::Native,
            "Launch container account workers through the host coordinator"
        );
        ensure!(
            account.uid == self.destination.uid,
            "Container worker belongs to another host account"
        );
        for pair in args.windows(2) {
            if pair[0] == "--config" || pair[0] == "--expected-config" {
                self.destination.check_config(Path::new(pair[1]))?;
            } else if pair[0] == "--working-directory" {
                ensure!(
                    Path::new(pair[1]) == self.destination.working_directory,
                    "Container worker working directory differs from preflight"
                );
            }
        }
        let args = self.launch.arguments(Some(&self.destination), args)?;
        let args: Vec<_> = args.iter().map(OsString::as_os_str).collect();
        account.box_command(&args)
    }

    pub fn ensure_separate(&self, other: &Self) -> Result<()> {
        self.destination.validate()?;
        other.destination.validate()?;
        ensure!(
            self.launch == other.launch
                && self.destination.name == other.destination.name
                && self.destination.scope != other.destination.scope
                && self.destination.uid == other.destination.uid
                && self.destination.mount_namespace == other.destination.mount_namespace,
            "Container service modes must use the same verified box and owner"
        );
        ensure!(
            self.destination.config_path.parent() != other.destination.config_path.parent()
                && self.destination.state_directory != other.destination.state_directory,
            "User and system modes must use separate state directories"
        );
        Ok(())
    }
}

static WORKER: OnceLock<Identity> = OnceLock::new();

pub fn initialize_worker(text: &str) -> Result<()> {
    ensure!(
        text.len() <= 16 * 1024,
        "Container destination metadata exceeds 16 KiB"
    );
    let identity: Identity = serde_json::from_str(text)?;
    identity.verify()?;
    WORKER
        .set(identity)
        .map_err(|_| anyhow::anyhow!("Container worker destination was already set"))
}

pub(crate) fn current() -> Option<&'static Identity> {
    WORKER.get()
}

impl Identity {
    pub(crate) fn check_config(&self, config: &Path) -> Result<()> {
        ensure!(
            self.config_path == config,
            "Container state operation targets another configuration"
        );
        Ok(())
    }
}

pub(crate) fn verify_config(config: &Path) -> Result<()> {
    if let Some(identity) = current() {
        identity.verify()?;
        identity.check_config(config)?;
    }
    Ok(())
}

pub(crate) fn current_account(name: &str) -> Result<Account> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Distrobox { name: name.into() },
        "Run destination inspection inside the selected Distrobox"
    );
    let mut account = Account::user(unsafe { libc::geteuid() })?;
    account.gid = unsafe { libc::getegid() };
    account.groups = crate::account::current_groups(account.gid)?;
    Ok(account)
}

pub fn inspect_identity(
    name: &str,
    scope: ServiceScope,
    config: &Path,
    working: &Path,
) -> Result<Identity> {
    validate_config(config)?;
    absolute(working)?;
    let account = current_account(name)?;
    let directory =
        std::fs::symlink_metadata(config.parent().context("Configuration has no parent")?)?;
    let identity = Identity {
        name: name.into(),
        scope,
        uid: account.uid,
        gid: account.gid,
        groups_fingerprint: account.group_fingerprint(),
        mount_namespace: namespace()?,
        state_directory: FileIdentity {
            device: directory.dev().to_string(),
            inode: directory.ino().to_string(),
        },
        config_path: config.into(),
        working_directory: working.into(),
    };
    identity.verify()?;
    Ok(identity)
}

fn namespace() -> Result<FileIdentity> {
    let metadata = std::fs::metadata("/proc/self/ns/mnt")?;
    Ok(FileIdentity {
        device: metadata.dev().to_string(),
        inode: metadata.ino().to_string(),
    })
}

pub(crate) fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.as_os_str().len() <= 4096
            && path
                .components()
                .all(|value| matches!(value, Component::RootDir | Component::Normal(_))),
        "Container destination paths must be absolute and contain no parent components"
    );
    Ok(())
}

pub(crate) fn validate_config(path: &Path) -> Result<()> {
    absolute(path)?;
    crate::state_transfer::validate_config_name(
        path.file_name()
            .and_then(OsStr::to_str)
            .context("Configuration must have a UTF-8 filename")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution() -> Execution {
        Execution {
            launch: Launch {
                name: "fixture box".into(),
                host_enter: "/usr/bin/distrobox-enter".into(),
                binaries: "/opt/app with spaces".into(),
            },
            destination: Identity {
                name: "fixture box".into(),
                scope: ServiceScope::User,
                uid: 1000,
                gid: 1000,
                groups_fingerprint: "a".repeat(64),
                mount_namespace: FileIdentity {
                    device: "4".into(),
                    inode: "5".into(),
                },
                state_directory: FileIdentity {
                    device: "6".into(),
                    inode: "7".into(),
                },
                config_path: "/home/user/state/user/custom.json".into(),
                working_directory: "/home/user".into(),
            },
        }
    }

    #[test]
    fn same_owner_modes_require_distinct_state_directories_in_the_same_box() {
        let user = execution();
        let mut system = user.clone();
        system.destination.scope = ServiceScope::System;
        system.destination.config_path = "/home/user/state/system/config.json".into();
        assert!(user.ensure_separate(&system).is_err());
        system.destination.state_directory.inode = "8".into();
        user.ensure_separate(&system).unwrap();
        system.destination.config_path = user.destination.config_path.clone();
        assert!(user.ensure_separate(&system).is_err());
        system.destination.config_path = "/home/user/state/system/config.json".into();
        system.destination.uid = 1001;
        assert!(user.ensure_separate(&system).is_err());
        system.destination.uid = 1000;
        system.destination.mount_namespace.inode = "other".into();
        assert!(user.ensure_separate(&system).is_err());
    }

    #[test]
    fn launch_arguments_keep_paths_and_destination_metadata_as_single_arguments() {
        let execution = execution();
        let args = execution
            .launch
            .arguments(
                Some(&execution.destination),
                &[
                    OsStr::new("check-saved-state"),
                    OsStr::new("--config"),
                    OsStr::new("/a path/$(literal).json"),
                ],
            )
            .unwrap();
        assert_eq!(args[5], OsStr::new("--binaries"));
        assert_eq!(args[6], OsStr::new("/opt/app with spaces"));
        let index = args
            .iter()
            .position(|value| value == "--destination")
            .unwrap();
        let decoded: Identity = serde_json::from_str(args[index + 1].to_str().unwrap()).unwrap();
        assert_eq!(decoded, execution.destination);
        assert_eq!(args.last().unwrap(), OsStr::new("/a path/$(literal).json"));
        let mut wrong = execution.destination;
        wrong.name = "another box".into();
        assert!(execution.launch.arguments(Some(&wrong), &[]).is_err());
    }

    #[test]
    fn destination_reports_must_match_credentials_namespace_paths_and_directory_identity() {
        let identity = execution().destination;
        identity.check_config(&identity.config_path).unwrap();
        assert!(identity
            .check_config(Path::new("/home/user/state/system/config.json"))
            .is_err());
        let mut report = Destination {
            scope: identity.scope,
            uid: identity.uid,
            gid: identity.gid,
            groups_fingerprint: identity.groups_fingerprint.clone(),
            mount_namespace: identity.mount_namespace.clone(),
            state_directory: Some(identity.state_directory.clone()),
            config_path: identity.config_path.clone(),
            working_directory: identity.working_directory.clone(),
            state: None,
            assets: lianli_shared::media_dependencies::AssetAccessReport {
                uid: 1000,
                checked: 0,
                failed: 0,
                issues: vec![],
            },
        };
        assert!(identity.matches(&report));
        report.state_directory = None;
        assert!(!identity.matches(&report));
        report.state_directory = Some(identity.state_directory.clone());
        report.groups_fingerprint = "b".repeat(64);
        assert!(!identity.matches(&report));
        report.groups_fingerprint = identity.groups_fingerprint.clone();
        report.mount_namespace.inode = "123".into();
        assert!(!identity.matches(&report));
    }

    #[test]
    fn destination_paths_preserve_custom_filenames_but_reject_reserved_or_ambiguous_targets() {
        validate_config(Path::new("/state/my-settings.json")).unwrap();
        for path in [
            "relative.json",
            "/state/../config.json",
            "/state/profiles",
            "/state/lcd_templates.json",
            "/",
        ] {
            assert!(validate_config(Path::new(path)).is_err());
        }
        let mut identity = execution().destination;
        identity.uid = 0;
        assert!(identity.validate().is_err());
    }
}
