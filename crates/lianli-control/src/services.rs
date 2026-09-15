use crate::command::{self, Output};
use anyhow::{bail, ensure, Context, Result};
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationContext, InstallationFinding, InstallationGuide,
};
use lianli_shared::services::{ServiceProbe, ServiceReport, UnitState, SYSTEM_UNIT, USER_UNIT};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROPERTIES: &str =
    "Id,LoadState,ActiveState,SubState,UnitFileState,MainPID,FragmentPath,ControlGroup,InvocationID,KillMode,SendSIGKILL,KillSignal,RestartKillSignal,TimeoutStopFailureMode,ExecStopPre,ExecStop,ExecStopPost";

pub(crate) enum Route {
    Native,
    Host { executable: PathBuf, bus: PathBuf },
}

impl Route {
    pub(crate) fn detect(context: &InstallationContext) -> Result<Self> {
        match context {
            InstallationContext::Native => Ok(Self::Native),
            InstallationContext::UnsupportedContainer => {
                bail!("Host service access is unavailable in this container")
            }
            InstallationContext::Distrobox { .. } => {
                let executable = ["/usr/bin/host-spawn", "/usr/local/bin/host-spawn"]
                    .into_iter().map(PathBuf::from).find(|path| existing_bridge(path).is_ok())
                    .context("Run distrobox-host-exec true interactively inside this box to install host-spawn, then Recheck. No helper is installed by this check.")?;
                let uid = unsafe { libc::geteuid() };
                let bus = PathBuf::from(format!("/run/host/run/user/{uid}/bus"));
                let metadata = fs::symlink_metadata(&bus)
                    .context("The host user's session bus is not visible in this box")?;
                ensure!(
                    metadata.file_type().is_socket() && metadata.uid() == uid,
                    "The visible host session bus does not belong to this user"
                );
                for name in ["systemctl", "timeout"] {
                    let path = format!("/run/host/usr/bin/{name}");
                    ensure!(
                        fs::metadata(&path).is_ok_and(
                            |metadata| metadata.is_file() && metadata.mode() & 0o111 != 0
                        ),
                        "Host /usr/bin/{name} is unavailable. Service checks cannot run safely"
                    );
                }
                Ok(Self::Host { executable, bus })
            }
        }
    }

    fn command(&self, program: &str, args: &[&str]) -> Command {
        self.command_with_timeout(program, args, Duration::from_secs(4))
    }

    fn command_with_timeout(&self, program: &str, args: &[&str], timeout: Duration) -> Command {
        let mut command = match self {
            Self::Native => {
                let mut command = Command::new(program);
                let runtime = format!("/run/user/{}", unsafe { libc::geteuid() });
                command
                    .env(
                        "DBUS_SESSION_BUS_ADDRESS",
                        format!("unix:path={runtime}/bus"),
                    )
                    .env("XDG_RUNTIME_DIR", runtime)
                    .env(
                        "DBUS_SYSTEM_BUS_ADDRESS",
                        "unix:path=/run/dbus/system_bus_socket",
                    );
                command
            }
            Self::Host { executable, bus } => {
                let mut command = Command::new(executable);
                command.args([
                    "--no-pty",
                    "--cwd=/",
                    "--env=LC_ALL,SYSTEMD_PAGER,SYSTEMD_COLORS",
                    "/usr/bin/timeout",
                    "--signal=TERM",
                    "--kill-after=1s",
                    &format!("{}s", timeout.as_secs()),
                    program,
                ]);
                command.env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", bus.display()),
                );
                command
            }
        };
        command
            .args(args)
            .env("LC_ALL", "C")
            .env("SYSTEMD_PAGER", "")
            .env("SYSTEMD_COLORS", "0");
        command
    }

    fn query(&self, args: &[&str]) -> Result<Output> {
        let mut options = vec!["--no-pager", "--no-ask-password"];
        options.extend_from_slice(args);
        self.output("/usr/bin/systemctl", &options)
    }

    pub(crate) fn output(&self, program: &str, args: &[&str]) -> Result<Output> {
        self.output_with_limit(program, args, 64 * 1024)
    }

    pub(crate) fn output_with_limit(
        &self,
        program: &str,
        args: &[&str],
        limit: usize,
    ) -> Result<Output> {
        let output = command::run_with_stdin_limit(
            self.command(program, args),
            std::process::Stdio::null(),
            Duration::from_secs(7),
            limit,
        )?;
        if matches!(self, Self::Host { .. }) && output.status.code() == Some(127) {
            bail!("Host command bridge failed: {}. Verify the host's Flatpak session helper and the box's host session-bus access.", command_error(&output));
        }
        Ok(output)
    }

    pub(crate) fn diagnostic_output(&self, program: &str, args: &[&str]) -> Result<Output> {
        command::run(
            self.command_with_timeout(program, args, Duration::from_secs(1)),
            Duration::from_secs(2),
        )
    }

    pub(crate) fn bounded_output(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output> {
        command::run_with_stdin_limit(
            self.command_with_timeout(program, args, timeout),
            std::process::Stdio::null(),
            timeout + Duration::from_secs(3),
            64 * 1024,
        )
    }

    pub(crate) fn service_action(
        &self,
        request: lianli_shared::services::ServiceActionRequest,
    ) -> Result<()> {
        let args = [
            request.scope.argument(),
            "--no-pager",
            "--no-block",
            "--job-mode=fail",
            request.action.argument(),
            request.scope.unit(),
        ];
        let command =
            self.command_with_timeout("/usr/bin/systemctl", &args, Duration::from_secs(120));
        let output = command::run(command, Duration::from_secs(123))?;
        ensure!(
            output.status.success(),
            "Service request failed: {}",
            command_error(&output)
        );
        Ok(())
    }
}

fn existing_bridge(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.mode() & 0o111 != 0,
        "Host bridge is not an executable file"
    );
    let mut header = [0; 4];
    file.read_exact(&mut header)?;
    ensure!(
        &header == b"\x7fELF",
        "Host bridge must be an installed binary, not an installation wrapper"
    );
    Ok(())
}

pub fn inspect(context: &InstallationContext) -> ServiceReport {
    let route = Route::detect(context);
    let unavailable = || ServiceProbe::Unavailable {
        reason: format!("{:#}", route.as_ref().err().unwrap()),
    };
    let Ok(route) = route.as_ref() else {
        return ServiceReport {
            context: context.clone(),
            user: unavailable(),
            system: unavailable(),
            global_user: ServiceProbe::Unavailable {
                reason: format!("{:#}", route.as_ref().err().unwrap()),
            },
            ownership: Some(ServiceProbe::Unavailable {
                reason: format!("{:#}", route.as_ref().err().unwrap()),
            }),
            operation_lock: Some(ServiceProbe::Unavailable {
                reason: format!("{:#}", route.as_ref().err().unwrap()),
            }),
            selection: Some(ServiceProbe::Unavailable {
                reason: format!("{:#}", route.as_ref().err().unwrap()),
            }),
        };
    };
    let unit = |scope, name| {
        probe(|| {
            let output = route.query(&[scope, "show", "--all", "--property", PROPERTIES, name])?;
            ensure!(output.status.success(), "{}", command_error(&output));
            let mut state = parse_unit(&output.stdout, name)?;
            if state.graceful_shutdown == Some(false)
                && output.stdout.lines().any(|line| {
                    line.strip_prefix("ExecStop=")
                        .is_some_and(|value| !value.is_empty())
                })
            {
                let service_scope = if name == USER_UNIT {
                    lianli_shared::services::ServiceScope::User
                } else {
                    lianli_shared::services::ServiceScope::System
                };
                if let Some(box_name) = crate::distrobox_service::inspect(route, service_scope)? {
                    state = parse_unit_with_stop(&output.stdout, name, true)?;
                    state.distrobox_name =
                        (state.graceful_shutdown == Some(true)).then_some(box_name);
                }
            }
            Ok(state)
        })
    };
    let mut report = ServiceReport {
        context: context.clone(),
        selection: Some(probe(|| crate::service_selection::inspect(context))),
        user: unit("--user", USER_UNIT),
        system: unit("--system", SYSTEM_UNIT),
        global_user: probe(|| {
            let output = route.query(&["--global", "is-enabled", USER_UNIT])?;
            parse_enablement(&output)
        }),
        ownership: Some(probe(|| crate::ownership::inspect(context, route))),
        operation_lock: Some(probe(|| {
            crate::reservation::operation_identity(context, route)
        })),
    };
    if let Some(ServiceProbe::Known { value }) = &mut report.ownership {
        if let Some(pid) = value.owner_pid {
            value.process = Some(probe(|| {
                let mut process =
                    crate::process_owner::inspect(route, pid, &report.user, &report.system)?;
                for (scope, unit) in [
                    (lianli_shared::services::ServiceScope::User, &report.user),
                    (
                        lianli_shared::services::ServiceScope::System,
                        &report.system,
                    ),
                ] {
                    let ServiceProbe::Known { value: wrapped } = unit else {
                        continue;
                    };
                    if process.service.is_none()
                        && wrapped.active_state == "active"
                        && wrapped.sub_state == "running"
                        && wrapped.distrobox_name.is_some()
                        && wrapped.invocation_id.is_some()
                    {
                        let expected_uid = match scope {
                            lianli_shared::services::ServiceScope::User => unsafe {
                                libc::geteuid()
                            },
                            lianli_shared::services::ServiceScope::System => {
                                crate::distrobox_service::system_owner_uid(route)?
                            }
                        };
                        if process.effective_uid != expected_uid {
                            continue;
                        }
                        let mut candidate = value.clone();
                        candidate.process = Some(ServiceProbe::Known {
                            value: process.clone(),
                        });
                        let info = crate::daemon_probe::inspect(context, scope, &candidate)?;
                        if wrapper_matches(&info, wrapped) {
                            process.service = Some(scope);
                        }
                    }
                }
                let current = crate::ownership::inspect(context, route)?;
                ensure!(
                    current.identity == value.identity && current.owner_pid == Some(pid),
                    "Hardware owner changed during process inspection. Recheck services"
                );
                Ok(process)
            }));
        }
    }
    report
}

fn wrapper_matches(info: &lianli_shared::daemon::DaemonInfo, unit: &UnitState) -> bool {
    unit.distrobox_name.is_some()
        && unit.invocation_id.is_some()
        && info.service_invocation == unit.invocation_id
        && info
            .capabilities
            .iter()
            .any(|capability| capability == lianli_shared::daemon::SERVICE_STOP)
}

pub fn findings(report: &ServiceReport) -> Vec<InstallationFinding> {
    let native = report.context == InstallationContext::Native;
    let mut findings = Vec::new();
    if native {
        if let Err(error) = crate::switch_job::prerequisites() {
            findings.push(InstallationFinding {
                code: "service.switch_prerequisites".into(), state: CheckState::Unavailable,
                severity: FindingSeverity::Warning, feature: "Service switching".into(), context: "Host".into(),
                title: "Service switching needs host support".into(), evidence: format!("{error:#}"),
                remediation: "Install the matching host control helper, systemd-run and Polkit. For Distrobox, use Set up host support in Settings. Run a desktop authentication agent, then Recheck.".into(),
                guide: InstallationGuide::ServiceModes,
            });
        }
    }
    let guide = if native {
        InstallationGuide::ServiceModes
    } else {
        InstallationGuide::Distrobox
    };
    let context = if native {
        "Host"
    } else {
        "Host via container bridge"
    };
    if let Some(ServiceProbe::Unavailable { reason }) = &report.selection {
        findings.push(InstallationFinding {
            code: "service.selection".into(), state: CheckState::Unavailable,
            severity: FindingSeverity::Error, feature: "Service startup".into(), context: context.into(),
            title: "Host service selection is unverified".into(), evidence: reason.clone(),
            remediation: "Resolve any unfinished service switch before starting a daemon. For invalid or hidden selection records, follow the Service modes guide and preserve recovery files. Do not create a private container selection file.".into(), guide,
        });
    }
    if let ServiceProbe::Known { value } = &report.user {
        let needs_recipe = match &report.context {
            InstallationContext::Distrobox { name } => {
                value.load_state == "not-found"
                    || (value.load_state == "loaded" && value.distrobox_name.as_ref() != Some(name))
            }
            InstallationContext::UnsupportedContainer => {
                value.load_state == "loaded" && value.distrobox_name.is_none()
            }
            InstallationContext::Native => false,
        };
        if needs_recipe {
            let remediation = match &report.context {
                InstallationContext::Distrobox { name } => crate::distrobox_unit::guidance(name)
                    .unwrap_or_else(|error| format!("{error:#}. Follow the Distrobox guide to select an existing box and update the host user unit.")),
                _ => "Follow the Distrobox guide to update the host user unit, then reload the user manager. Keep both application binaries installed inside the same box. Stop an older daemon cleanly before changing its unit.".into(),
            };
            findings.push(InstallationFinding {
                code: "service.distrobox_recipe".into(), state: CheckState::Failed,
                severity: FindingSeverity::Warning, feature: "Service management".into(), context: context.into(),
                title: "Distrobox service controls need the current service recipe".into(),
                evidence: "The host user service is missing or does not have verified invocation-aware start and stop commands for this box.".into(),
                remediation, guide,
            });
        }
    }
    if let Some(ServiceProbe::Unavailable { reason }) = &report.operation_lock {
        findings.push(InstallationFinding {
            code: "service.operation_lock".into(), state: CheckState::Unavailable,
            severity: FindingSeverity::Warning, feature: "Service management".into(),
            context: context.into(), title: "Service controls need the current host setup".into(),
            evidence: reason.clone(),
            remediation: "Install the current packaging/tmpfiles.d/lianli.conf on the host and run sudo systemd-tmpfiles --create lianli.conf, then Recheck.".into(), guide,
        });
    }
    if let Some(ServiceProbe::Unavailable { reason }) = &report.ownership {
        findings.push(InstallationFinding {
            code: "ownership.kernel".into(), state: CheckState::Unavailable, severity: FindingSeverity::Warning,
            feature: "Daemon ownership".into(), context: context.into(), title: "Kernel lock ownership is unverified".into(),
            evidence: reason.clone(), remediation: "Restore the shared host lock and host bridge before switching modes. A PID written in the file does not prove current ownership.".into(), guide,
        });
    }
    if let Some(ServiceProbe::Known { value }) = &report.ownership {
        if let Some(ServiceProbe::Unavailable { reason }) = &value.process {
            findings.push(InstallationFinding {
                code: "ownership.process".into(), state: CheckState::Unavailable,
                severity: FindingSeverity::Warning, feature: "Daemon ownership".into(),
                context: context.into(), title: "Hardware owner's service is unverified".into(),
                evidence: reason.clone(),
                remediation: "Recheck after startup or shutdown finishes. Service switching requires host process and service cgroup access. Do not stop a PID based on its number alone.".into(), guide,
            });
        }
    }
    for (code, title, probe) in [
        ("service.user", "User hardware service", &report.user),
        ("service.system", "System hardware service", &report.system),
    ] {
        let (state, severity, evidence) = match probe {
            ServiceProbe::Unavailable { reason } => (
                CheckState::Unavailable,
                FindingSeverity::Warning,
                reason.clone(),
            ),
            ServiceProbe::Known { value }
                if value.load_state == "not-found" && !native && code == "service.system" =>
            {
                (
                    CheckState::NotApplicable,
                    FindingSeverity::Info,
                    "System mode is not set up. Use Set up host support in Settings.".into(),
                )
            }
            ServiceProbe::Known { value } if value.load_state == "not-found" => (
                CheckState::Failed,
                FindingSeverity::Warning,
                format!("{} is not installed", value.name),
            ),
            ServiceProbe::Known { value } if value.active_state == "failed" => (
                CheckState::Failed,
                FindingSeverity::Error,
                format!(
                    "{} failed. Startup state is {}",
                    value.name, value.unit_file_state
                ),
            ),
            ServiceProbe::Known { value }
                if matches!(value.load_state.as_str(), "error" | "bad-setting") =>
            {
                (
                    CheckState::Failed,
                    FindingSeverity::Error,
                    format!("{} cannot load: {}", value.name, value.load_state),
                )
            }
            ServiceProbe::Known { value }
                if !matches!(value.load_state.as_str(), "loaded" | "masked") =>
            {
                (
                    CheckState::Unavailable,
                    FindingSeverity::Warning,
                    format!(
                        "{} has an unrecognized load state: {}",
                        value.name, value.load_state
                    ),
                )
            }
            ServiceProbe::Known { value } => (
                CheckState::Passed,
                FindingSeverity::Info,
                format!(
                    "{}: load {}, state {}/{}, startup {}",
                    value.name,
                    value.load_state,
                    value.active_state,
                    value.sub_state,
                    value.unit_file_state
                ),
            ),
        };
        findings.push(InstallationFinding {
            code: code.into(), state, severity, feature: "Service management".into(), context: context.into(),
            title: title.into(), evidence,
            remediation: "Check the host service setup and journal. Install the supplied units, then select one hardware mode. Do not start another daemon while an existing owner is running.".into(), guide,
        });
    }
    let global_enabled = match &report.global_user {
        ServiceProbe::Known { value } => value == "enabled" || value == "enabled-runtime",
        ServiceProbe::Unavailable { reason } => {
            findings.push(InstallationFinding {
                code: "service.global_user".into(),
                state: CheckState::Unavailable,
                severity: FindingSeverity::Warning,
                feature: "Service management".into(),
                context: context.into(),
                title: "Global user startup is unverified".into(),
                evidence: reason.clone(),
                remediation:
                    "Check global user-service enablement on the host before selecting system mode."
                        .into(),
                guide,
            });
            false
        }
    };
    let selected = |probe: &ServiceProbe<UnitState>| match probe {
        ServiceProbe::Known { value } => {
            matches!(
                value.unit_file_state.as_str(),
                "enabled" | "enabled-runtime"
            ) || !matches!(value.active_state.as_str(), "inactive" | "failed")
        }
        ServiceProbe::Unavailable { .. } => false,
    };
    if selected(&report.system) && (selected(&report.user) || global_enabled) {
        findings.push(InstallationFinding {
            code: "service.conflict".into(), state: CheckState::Failed, severity: FindingSeverity::Error,
            feature: "Daemon ownership".into(), context: context.into(), title: "Competing hardware service selections".into(),
            evidence: "Both hardware modes are active or selected for startup. The ownership lock prevents concurrent access, but competing services may keep retrying.".into(),
            remediation: "Choose one hardware service mode. Resolve global user enablement and manually launched owners before switching. Do not stop another user's daemon.".into(), guide,
        });
    } else if global_enabled {
        findings.push(InstallationFinding {
            code: "service.global_user".into(), state: CheckState::Failed, severity: FindingSeverity::Warning,
            feature: "Daemon ownership".into(), context: context.into(), title: "Hardware service enabled for every user".into(),
            evidence: "The host has global user-service enablement. Another login can start a competing hardware service.".into(),
            remediation: "Ask the administrator to disable the global default and select startup only for the intended user or the system service.".into(), guide,
        });
    }
    findings
}

fn probe<T>(query: impl FnOnce() -> Result<T>) -> ServiceProbe<T> {
    match query() {
        Ok(value) => ServiceProbe::Known { value },
        Err(error) => ServiceProbe::Unavailable {
            reason: format!("{error:#}"),
        },
    }
}

fn command_error(output: &Output) -> String {
    let message = output.stderr.trim();
    if message.is_empty() {
        format!("systemctl exited with {}", output.status)
    } else {
        message.chars().take(2048).collect()
    }
}

fn parse_enablement(output: &Output) -> Result<String> {
    let value = output.stdout.trim();
    ensure!(
        matches!(output.status.code(), Some(0 | 1))
            || (output.status.code() == Some(4) && value == "not-found"),
        "{}",
        command_error(output)
    );
    ensure!(
        matches!(
            value,
            "enabled"
                | "enabled-runtime"
                | "linked"
                | "linked-runtime"
                | "alias"
                | "masked"
                | "masked-runtime"
                | "static"
                | "disabled"
                | "indirect"
                | "generated"
                | "transient"
                | "not-found"
        ),
        "Cannot determine global user enablement: {}",
        command_error(output)
    );
    Ok(value.into())
}

fn parse_unit(text: &str, name: &str) -> Result<UnitState> {
    parse_unit_with_stop(text, name, false)
}

fn parse_unit_with_stop(text: &str, name: &str, verified_stop: bool) -> Result<UnitState> {
    let mut properties = HashMap::new();
    let mut nonempty_stop_hooks = std::collections::HashSet::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .context("Malformed systemctl property")?;
        if matches!(key, "ExecStopPre" | "ExecStop" | "ExecStopPost") {
            if !value.is_empty() {
                nonempty_stop_hooks.insert(key);
            }
            continue;
        }
        ensure!(
            properties.insert(key, value).is_none(),
            "Duplicate systemctl property {key}"
        );
    }
    let field = |key| {
        properties
            .get(key)
            .copied()
            .with_context(|| format!("Missing service property {key}"))
    };
    ensure!(
        field("Id")? == name,
        "The queried service resolves to another unit"
    );
    let load_state = field("LoadState")?;
    let pid = properties.get("MainPID").copied();
    ensure!(
        pid.is_some() || load_state == "not-found",
        "Missing service process identity"
    );
    Ok(UnitState {
        name: name.into(),
        load_state: load_state.into(),
        active_state: field("ActiveState")?.into(),
        sub_state: field("SubState")?.into(),
        unit_file_state: properties
            .get("UnitFileState")
            .filter(|value| !value.is_empty())
            .unwrap_or(&"unknown")
            .to_string(),
        main_pid: pid.unwrap_or("0").parse().context("Invalid service PID")?,
        fragment_path: properties.get("FragmentPath").unwrap_or(&"").to_string(),
        invocation_id: properties
            .get("InvocationID")
            .filter(|value| !value.is_empty() && !value.bytes().all(|byte| byte == b'0'))
            .map(|value| {
                lianli_shared::daemon::parse_service_invocation(value).map_err(anyhow::Error::msg)
            })
            .transpose()?,
        distrobox_name: None,
        control_group: properties
            .get("ControlGroup")
            .map(|value| value.to_string()),
        kill_mode: properties.get("KillMode").map(|value| value.to_string()),
        send_sigkill: properties
            .get("SendSIGKILL")
            .map(|value| match *value {
                "yes" => Ok(true),
                "no" => Ok(false),
                _ => anyhow::bail!("Invalid service kill policy"),
            })
            .transpose()?,
        // systemctl omits empty Exec* arrays and repeats properties for multiple commands.
        graceful_shutdown: ["KillSignal", "RestartKillSignal", "TimeoutStopFailureMode"]
            .iter()
            .all(|key| properties.contains_key(key))
            .then(|| {
                properties["KillSignal"] == "15"
                    && properties["RestartKillSignal"] == "15"
                    && properties["TimeoutStopFailureMode"] == "terminate"
                    && ["ExecStopPre", "ExecStop", "ExecStopPost"]
                        .iter()
                        .all(|key| {
                            (*key == "ExecStop" && verified_stop)
                                || !nonempty_stop_hooks.contains(key)
                        })
            }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_stop_commands_preserve_shutdown_validation() {
        let base = format!(
            "Id={USER_UNIT}\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nKillSignal=15\nRestartKillSignal=15\nTimeoutStopFailureMode=terminate\n"
        );
        let wrapper = format!("{base}ExecStop=stop daemon\nExecStop=wait for wrapper\n");
        assert_eq!(
            parse_unit(&wrapper, USER_UNIT).unwrap().graceful_shutdown,
            Some(false)
        );
        assert_eq!(
            parse_unit_with_stop(&wrapper, USER_UNIT, true)
                .unwrap()
                .graceful_shutdown,
            Some(true)
        );
        for hooks in [
            "ExecStopPost=custom\nExecStopPost=\n",
            "ExecStopPost=\nExecStopPost=custom\n",
        ] {
            assert_eq!(
                parse_unit_with_stop(&(wrapper.clone() + hooks), USER_UNIT, true)
                    .unwrap()
                    .graceful_shutdown,
                Some(false)
            );
        }
        assert!(parse_unit(&(base + "MainPID=1\n"), USER_UNIT).is_err());
    }
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn wrapper_association_requires_the_current_invocation_and_stop_capability() {
        let invocation = "abcdef0123456789abcdef0123456789";
        let mut unit = parse_unit(&format!("Id={USER_UNIT}\nLoadState=loaded\nActiveState=active\nSubState=running\nMainPID=123\nInvocationID={invocation}\n"), USER_UNIT).unwrap();
        let mut info: lianli_shared::daemon::DaemonInfo =
            serde_json::from_value(serde_json::json!({
                "version":"0.9.1", "protocol_version":1, "instance_id":"test", "pid":123,
                "mode":"user", "config_path":"/test/config.json", "service_invocation":invocation,
                "capabilities":[lianli_shared::daemon::SERVICE_STOP]
            }))
            .unwrap();
        assert!(!wrapper_matches(&info, &unit));
        unit.distrobox_name = Some("box".into());
        assert!(wrapper_matches(&info, &unit));
        info.service_invocation = Some("00000000000000000000000000000001".into());
        assert!(!wrapper_matches(&info, &unit));
        info.service_invocation = unit.invocation_id.clone();
        info.capabilities.clear();
        assert!(!wrapper_matches(&info, &unit));
    }

    #[test]
    fn shutdown_policy_rejects_forced_signals_custom_hooks_and_missing_evidence() {
        let base = format!(
            "Id={USER_UNIT}\nLoadState=loaded\nActiveState=active\nSubState=running\nMainPID=123\n"
        );
        let policy = "KillSignal=15\nRestartKillSignal=15\nTimeoutStopFailureMode=terminate\nExecStop=\nExecStopPost=\n";
        let wrapper = base.clone() + &policy.replace("ExecStop=\n", "ExecStop=verified wrapper\n");
        assert_eq!(
            parse_unit_with_stop(&wrapper, USER_UNIT, true)
                .unwrap()
                .graceful_shutdown,
            Some(true)
        );
        for invalid in [
            wrapper.replace("KillSignal=15", "KillSignal=9"),
            wrapper.replace("ExecStopPost=\n", "ExecStopPost=other command\n"),
        ] {
            assert_eq!(
                parse_unit_with_stop(&invalid, USER_UNIT, true)
                    .unwrap()
                    .graceful_shutdown,
                Some(false)
            );
        }
        assert_eq!(
            parse_unit(&(base.clone() + policy), USER_UNIT)
                .unwrap()
                .graceful_shutdown,
            Some(true)
        );
        let empty_hooks_omitted = policy
            .replace("ExecStop=\n", "")
            .replace("ExecStopPost=\n", "");
        assert_eq!(
            parse_unit(&(base.clone() + &empty_hooks_omitted), USER_UNIT)
                .unwrap()
                .graceful_shutdown,
            Some(true)
        );
        assert_eq!(
            parse_unit(
                &(base.clone() + &empty_hooks_omitted + "ExecStopPre=custom command\n"),
                USER_UNIT
            )
            .unwrap()
            .graceful_shutdown,
            Some(false)
        );
        for (before, after) in [
            ("KillSignal=15", "KillSignal=9"),
            ("RestartKillSignal=15", "RestartKillSignal=9"),
            ("FailureMode=terminate", "FailureMode=kill"),
            ("ExecStop=\n", "ExecStop=custom command\n"),
            ("ExecStopPost=\n", "ExecStopPost=custom command\n"),
        ] {
            assert_eq!(
                parse_unit(&(base.clone() + &policy.replace(before, after)), USER_UNIT)
                    .unwrap()
                    .graceful_shutdown,
                Some(false)
            );
        }
        assert_eq!(
            parse_unit(&base, USER_UNIT).unwrap().graceful_shutdown,
            None
        );
    }

    #[test]
    fn missing_and_failed_services_are_distinct_from_unavailable_queries() {
        let missing =
            format!("Id={USER_UNIT}\nLoadState=not-found\nActiveState=inactive\nSubState=dead\n");
        assert_eq!(
            parse_unit(&missing, USER_UNIT).unwrap().load_state,
            "not-found"
        );
        let failed = format!("Id={USER_UNIT}\nLoadState=loaded\nActiveState=failed\nSubState=failed\nMainPID=0\nUnitFileState=enabled\nFragmentPath=/home/a b/unit\n");
        let state = parse_unit(&failed, USER_UNIT).unwrap();
        assert_eq!(state.active_state, "failed");
        assert_eq!(state.unit_file_state, "enabled");
        assert_eq!(state.fragment_path, "/home/a b/unit");
        assert!(parse_unit(&failed, SYSTEM_UNIT).is_err());
        assert!(parse_unit(&(failed + "MainPID=123\n"), USER_UNIT).is_err());
        assert!(parse_unit("Failed to connect to bus", USER_UNIT).is_err());
    }

    #[test]
    fn disabled_global_unit_is_known_despite_nonzero_exit() {
        let mut output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: "disabled\n".into(),
            stderr: String::new(),
        };
        assert_eq!(parse_enablement(&output).unwrap(), "disabled");
        output.stdout.clear();
        assert!(parse_enablement(&output).is_err());
        output.status = std::process::ExitStatus::from_raw(127 << 8);
        output.stdout = "disabled\n".into();
        assert!(parse_enablement(&output).is_err());
    }

    #[test]
    fn missing_global_unit_is_known_only_with_a_valid_missing_result() {
        let mut output = Output {
            status: std::process::ExitStatus::from_raw(4 << 8),
            stdout: "not-found\n".into(),
            stderr: String::new(),
        };
        assert_eq!(parse_enablement(&output).unwrap(), "not-found");
        output.stdout = "disabled\n".into();
        assert!(parse_enablement(&output).is_err());
        output.stdout = "not-found\n".into();
        output.status = std::process::ExitStatus::from_raw(127 << 8);
        assert!(parse_enablement(&output).is_err());
    }

    #[test]
    fn bridge_rejects_installation_wrappers_and_symlinks_without_execution() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("host-spawn");
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(existing_bridge(&path).is_err());
        fs::remove_file(&path).unwrap();
        symlink("/bin/true", &path).unwrap();
        assert!(existing_bridge(&path).is_err());
    }

    #[test]
    fn distrobox_setup_guidance_tracks_missing_and_wrong_box_units() {
        let mut report: ServiceReport = serde_json::from_value(serde_json::json!({
            "context": {"kind": "distrobox", "name": "my box"},
            "user": {"state": "known", "value": {
                "name": USER_UNIT, "load_state": "not-found", "active_state": "inactive",
                "sub_state": "dead", "unit_file_state": "disabled", "main_pid": 0, "fragment_path": ""
            }},
            "system": {"state": "unavailable", "reason": "fixture"},
            "global_user": {"state": "known", "value": "disabled"}
        })).unwrap();
        let finding = findings(&report)
            .into_iter()
            .find(|finding| finding.code == "service.distrobox_recipe")
            .unwrap();
        assert!(finding.remediation.contains("--box 'my box'"));
        for (name, expected) in [(None, true), (Some("other"), true), (Some("my box"), false)] {
            let ServiceProbe::Known { value } = &mut report.user else {
                unreachable!()
            };
            value.load_state = "loaded".into();
            value.distrobox_name = name.map(str::to_owned);
            assert_eq!(
                findings(&report)
                    .iter()
                    .any(|finding| finding.code == "service.distrobox_recipe"),
                expected
            );
        }
    }

    #[test]
    fn global_enablement_conflicts_with_system_startup_even_if_user_is_stopped() {
        let unit = |name: &str, startup: &str| ServiceProbe::Known {
            value: UnitState {
                name: name.into(),
                load_state: "loaded".into(),
                active_state: "inactive".into(),
                sub_state: "dead".into(),
                unit_file_state: startup.into(),
                main_pid: 0,
                fragment_path: String::new(),
                control_group: None,
                kill_mode: None,
                send_sigkill: None,
                graceful_shutdown: None,
                invocation_id: None,
                distrobox_name: None,
            },
        };
        let mut report = ServiceReport {
            context: InstallationContext::Native,
            user: unit(USER_UNIT, "disabled"),
            system: unit(SYSTEM_UNIT, "enabled"),
            global_user: ServiceProbe::Known {
                value: "enabled".into(),
            },
            ownership: None,
            operation_lock: None,
            selection: None,
        };
        assert!(findings(&report)
            .iter()
            .any(|finding| finding.code == "service.conflict"));
        report.global_user = ServiceProbe::Known {
            value: "disabled".into(),
        };
        assert!(!findings(&report)
            .iter()
            .any(|finding| finding.code == "service.conflict"));
        report.user = ServiceProbe::Unavailable {
            reason: "bus unavailable".into(),
        };
        assert!(findings(&report)
            .iter()
            .any(|finding| finding.code == "service.user"
                && finding.state == CheckState::Unavailable));
    }
}
