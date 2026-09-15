use crate::account::Account;
use crate::reservation::{HardwareReservation, ServiceOperationLock};
use crate::switch_journal::Startup;
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceProbe, ServiceReport, ServiceScope};
use std::ffi::OsStr;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    pub user: Startup,
    pub system: Startup,
}

pub fn inspect() -> Result<Policy> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native
            && unsafe { libc::geteuid() } != 0,
        "Inspect native startup selection under the caller account"
    );
    for scope in [ServiceScope::User, ServiceScope::System] {
        crate::destination::verify_service_recipe(scope)?;
    }
    Policy::from_report(&crate::services::inspect(&InstallationContext::Native))
}

impl Policy {
    pub fn from_report(report: &ServiceReport) -> Result<Self> {
        ensure!(
            report.context == InstallationContext::Native,
            "Startup selection requires native services"
        );
        match &report.global_user {
            ServiceProbe::Known { value } if value == "disabled" => {}
            _ => anyhow::bail!("Disable global user-service enablement and verify it before switching. Other users' startup settings will not be changed"),
        }
        Ok(Self {
            user: read_startup(&report.user)?,
            system: read_startup(&report.system)?,
        })
    }

    pub fn validate(self) -> Result<()> {
        ensure!(
            self.user == Startup::Disabled || self.system == Startup::Disabled,
            "Only one hardware service may be enabled for startup"
        );
        Ok(())
    }

    fn get(self, scope: ServiceScope) -> Startup {
        match scope {
            ServiceScope::User => self.user,
            ServiceScope::System => self.system,
        }
    }

    fn set(&mut self, scope: ServiceScope, value: Startup) {
        match scope {
            ServiceScope::User => self.user = value,
            ServiceScope::System => self.system = value,
        }
    }

    pub(crate) fn restore_for_boot(self, original: Option<&str>, current: &str) -> Result<Self> {
        self.validate()?;
        if self.user != Startup::Runtime && self.system != Startup::Runtime {
            return Ok(self);
        }
        let original = original.context("The original boot for runtime-only startup is unknown. Preserve the journal and recover manually")?;
        let expire = |value| {
            if original != current && value == Startup::Runtime {
                Startup::Disabled
            } else {
                value
            }
        };
        Ok(Self {
            user: expire(self.user),
            system: expire(self.system),
        })
    }
}

pub(crate) fn read_startup(
    probe: &ServiceProbe<lianli_shared::services::UnitState>,
) -> Result<Startup> {
    let ServiceProbe::Known { value } = probe else {
        anyhow::bail!("Service enablement is unverified")
    };
    ensure!(
        value.load_state == "loaded",
        "Install and reload the native service unit before recovery"
    );
    match value.unit_file_state.as_str() {
        "disabled" => Ok(Startup::Disabled),
        "enabled" => Ok(Startup::Enabled),
        "enabled-runtime" => Ok(Startup::Runtime),
        other => anyhow::bail!("Unsupported startup state {other} for {}. Preserve overrides and resolve them before switching", value.name),
    }
}

pub(crate) fn restore_system(
    operation: &ServiceOperationLock,
    hardware: &HardwareReservation,
    operation_id: &str,
    desired: Startup,
) -> Result<()> {
    let verify = || -> Result<()> {
        operation.verify()?;
        hardware.verify()?;
        crate::service_selection::require_paused(operation_id)?;
        crate::destination::verify_service_recipe(ServiceScope::System)
    };
    let inspect = || -> Result<Startup> {
        verify()?;
        read_startup(&crate::services::inspect(&InstallationContext::Native).system)
    };
    run_system(
        inspect,
        |change| {
            verify()?;
            let output = crate::services::Route::Native.output(
                "/usr/bin/systemctl",
                &change.arguments(ServiceScope::System),
            )?;
            verify()?;
            ensure!(output.status.success(), "System startup restoration was not confirmed: {}. Inspect its state before requesting recovery again", output.stderr.trim());
            Ok(())
        },
        desired,
    )
}

fn run_system(
    mut inspect: impl FnMut() -> Result<Startup>,
    mut change: impl FnMut(Change) -> Result<()>,
    desired: Startup,
) -> Result<()> {
    let original = inspect()?;
    if original == desired {
        return Ok(());
    }
    if original != Startup::Disabled {
        change(Change::Disable { runtime: true })?;
        change(Change::Disable { runtime: false })?;
    }
    ensure!(
        inspect()? == Startup::Disabled,
        "System startup disablement could not be verified. Startup remains paused"
    );
    if desired != Startup::Disabled {
        change(Change::Enable {
            runtime: desired == Startup::Runtime,
        })?;
    }
    ensure!(
        inspect()? == desired,
        "System startup restoration could not be verified. Startup remains paused"
    );
    Ok(())
}

pub(crate) fn boot_id() -> Result<String> {
    let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    lianli_shared::daemon::parse_service_invocation(&text.trim().replace('-', ""))
        .map_err(anyhow::Error::msg)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Disable { runtime: bool },
    Enable { runtime: bool },
}

impl Change {
    fn arguments(self, scope: ServiceScope) -> Vec<&'static str> {
        let (verb, runtime) = match self {
            Self::Disable { runtime } => ("disable", runtime),
            Self::Enable { runtime } => ("enable", runtime),
        };
        let mut args = vec![scope.argument(), "--no-ask-password", "--no-pager"];
        if runtime {
            args.push("--runtime");
        }
        args.extend([verb, scope.unit()]);
        args
    }
}

trait Backend {
    fn inspect(&mut self) -> Result<Policy>;
    fn change(&mut self, scope: ServiceScope, change: Change) -> Result<()>;
}

fn run(backend: &mut impl Backend, desired: Policy) -> Result<()> {
    desired.validate()?;
    let original = backend.inspect()?;
    let scopes = [ServiceScope::User, ServiceScope::System];
    let mut disabled = original;
    for scope in scopes {
        if original.get(scope) != desired.get(scope) && original.get(scope) != Startup::Disabled {
            backend.change(scope, Change::Disable { runtime: true })?;
            backend.change(scope, Change::Disable { runtime: false })?;
            disabled.set(scope, Startup::Disabled);
        }
    }
    ensure!(
        backend.inspect()? == disabled,
        "Service disablement changed unexpectedly. Startup remains paused for recovery"
    );
    for scope in scopes {
        if desired.get(scope) != Startup::Disabled && disabled.get(scope) != desired.get(scope) {
            backend.change(
                scope,
                Change::Enable {
                    runtime: desired.get(scope) == Startup::Runtime,
                },
            )?;
        }
    }
    ensure!(
        backend.inspect()? == desired,
        "Service startup selection could not be verified. Startup remains paused for recovery"
    );
    Ok(())
}

pub(crate) fn apply(
    caller: &Account,
    operation: &ServiceOperationLock,
    hardware: &HardwareReservation,
    operation_id: &str,
    desired: Policy,
) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0
            && InstallationContext::detect() == InstallationContext::Native,
        "Startup changes require the authorized native coordinator"
    );
    ensure!(
        std::env::current_exe()?.file_name() == Some(OsStr::new("lianli-control")),
        "Use the standalone coordinator for startup selection"
    );
    desired.validate()?;
    let mut backend = Native {
        caller,
        operation,
        hardware,
        operation_id,
    };
    if backend.inspect()? == desired {
        return Ok(());
    }
    crate::service_selection::require_paused(operation_id)?;
    run(&mut backend, desired)
}

struct Native<'a> {
    caller: &'a Account,
    operation: &'a ServiceOperationLock,
    hardware: &'a HardwareReservation,
    operation_id: &'a str,
}

impl Native<'_> {
    fn verify(&self) -> Result<()> {
        self.operation.verify()?;
        self.hardware.verify()
    }
}

impl Backend for Native<'_> {
    fn inspect(&mut self) -> Result<Policy> {
        self.verify()?;
        let output = crate::command::run(
            self.caller
                .control_command(&[OsStr::new("inspect-startup")])?,
            Duration::from_secs(60),
        )?;
        self.verify()?;
        ensure!(
            output.status.success(),
            "Cannot inspect caller startup selection: {}",
            output.stderr.trim()
        );
        serde_json::from_str(&output.stdout).context("Invalid inspected startup policy")
    }

    fn change(&mut self, scope: ServiceScope, change: Change) -> Result<()> {
        self.verify()?;
        crate::service_selection::require_paused(self.operation_id)?;
        let args = change.arguments(scope);
        let output = match scope {
            ServiceScope::User => crate::command::run(
                self.caller.command(
                    "/usr/bin/systemctl",
                    &args.iter().map(OsStr::new).collect::<Vec<_>>(),
                )?,
                Duration::from_secs(30),
            )?,
            ServiceScope::System => {
                crate::services::Route::Native.output("/usr/bin/systemctl", &args)?
            }
        };
        self.verify()?;
        ensure!(
            output.status.success(),
            "Startup change was not retried: {}. Recheck enablement before recovery",
            output.stderr.trim()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_restoration_changes_only_system_enablement_and_verifies_each_stage() {
        use std::cell::RefCell;
        for original in [[false, false], [true, false], [false, true], [true, true]] {
            for desired in [Startup::Disabled, Startup::Enabled, Startup::Runtime] {
                let fixture = RefCell::new(fixture([true, true], original));
                run_system(
                    || Ok(fixture.borrow().policy().system),
                    |change| fixture.borrow_mut().change(ServiceScope::System, change),
                    desired,
                )
                .unwrap();
                assert_eq!(fixture.borrow().policy().system, desired);
                assert_eq!(fixture.borrow().user, [true, true]);
                let previous_calls = fixture.borrow().calls.len();
                run_system(
                    || Ok(fixture.borrow().policy().system),
                    |change| fixture.borrow_mut().change(ServiceScope::System, change),
                    desired,
                )
                .unwrap();
                assert_eq!(fixture.borrow().calls.len(), previous_calls);
            }
        }
        let fixture = RefCell::new(fixture([false, false], [true, true]));
        fixture.borrow_mut().ignore_disable = true;
        assert!(run_system(
            || Ok(fixture.borrow().policy().system),
            |change| fixture.borrow_mut().change(ServiceScope::System, change),
            Startup::Runtime
        )
        .is_err());
        assert!(fixture
            .borrow()
            .calls
            .iter()
            .all(|(_, change)| matches!(change, Change::Disable { .. })));
        fixture.borrow_mut().ignore_disable = false;
        fixture.borrow_mut().calls.clear();
        fixture.borrow_mut().fail_at = Some(1);
        assert!(run_system(
            || Ok(fixture.borrow().policy().system),
            |change| fixture.borrow_mut().change(ServiceScope::System, change),
            Startup::Runtime
        )
        .is_err());
        assert_eq!(fixture.borrow().calls.len(), 1);
    }

    struct Fixture {
        user: [bool; 2],
        system: [bool; 2],
        calls: Vec<(ServiceScope, Change)>,
        fail_at: Option<usize>,
        ignore_disable: bool,
    }

    impl Fixture {
        fn policy(&self) -> Policy {
            let state = |layers: [bool; 2]| {
                if layers[0] {
                    Startup::Enabled
                } else if layers[1] {
                    Startup::Runtime
                } else {
                    Startup::Disabled
                }
            };
            Policy {
                user: state(self.user),
                system: state(self.system),
            }
        }
    }

    impl Backend for Fixture {
        fn inspect(&mut self) -> Result<Policy> {
            Ok(self.policy())
        }
        fn change(&mut self, scope: ServiceScope, change: Change) -> Result<()> {
            self.calls.push((scope, change));
            let layers = match scope {
                ServiceScope::User => &mut self.user,
                ServiceScope::System => &mut self.system,
            };
            match change {
                Change::Disable { runtime } if !self.ignore_disable => {
                    layers[usize::from(runtime)] = false
                }
                Change::Disable { .. } => {}
                Change::Enable { runtime } => layers[usize::from(runtime)] = true,
            }
            ensure!(
                self.fail_at != Some(self.calls.len()),
                "Fixture lost the command acknowledgement"
            );
            Ok(())
        }
    }

    fn fixture(user: [bool; 2], system: [bool; 2]) -> Fixture {
        Fixture {
            user,
            system,
            calls: Vec::new(),
            fail_at: None,
            ignore_disable: false,
        }
    }

    #[test]
    fn switching_clears_both_enablement_layers_before_enabling_the_destination() {
        let mut backend = fixture([true, true], [false, false]);
        let desired = Policy {
            user: Startup::Disabled,
            system: Startup::Enabled,
        };
        run(&mut backend, desired).unwrap();
        assert_eq!(
            backend.calls,
            [
                (ServiceScope::User, Change::Disable { runtime: true }),
                (ServiceScope::User, Change::Disable { runtime: false }),
                (ServiceScope::System, Change::Enable { runtime: false }),
            ]
        );
        assert_eq!(backend.user, [false, false]);
        assert_eq!(backend.system, [true, false]);
        backend.calls.clear();
        run(&mut backend, desired).unwrap();
        assert!(backend.calls.is_empty());
        let restore = Policy {
            user: Startup::Runtime,
            system: Startup::Disabled,
        };
        run(&mut backend, restore).unwrap();
        assert_eq!(backend.user, [false, true]);
        assert_eq!(backend.system, [false, false]);
    }

    #[test]
    fn interrupted_enablement_can_be_recovered_without_replaying_a_failed_call() {
        for failed in 1..=3 {
            let mut backend = fixture([true, true], [false, false]);
            backend.fail_at = Some(failed);
            assert!(run(
                &mut backend,
                Policy {
                    user: Startup::Disabled,
                    system: Startup::Enabled
                }
            )
            .is_err());
            assert_eq!(backend.calls.len(), failed);
            backend.fail_at = None;
            run(
                &mut backend,
                Policy {
                    user: Startup::Enabled,
                    system: Startup::Disabled,
                },
            )
            .unwrap();
            assert_eq!(
                backend.policy(),
                Policy {
                    user: Startup::Enabled,
                    system: Startup::Disabled
                }
            );
        }
    }

    #[test]
    fn unverified_disablement_never_enables_the_other_service() {
        let mut backend = fixture([true, false], [false, false]);
        backend.ignore_disable = true;
        assert!(run(
            &mut backend,
            Policy {
                user: Startup::Disabled,
                system: Startup::Runtime
            }
        )
        .is_err());
        assert!(backend
            .calls
            .iter()
            .all(|(_, change)| matches!(change, Change::Disable { .. })));
        backend.calls.clear();
        assert!(run(
            &mut backend,
            Policy {
                user: Startup::Enabled,
                system: Startup::Enabled
            }
        )
        .is_err());
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn runtime_startup_expires_across_boots_and_unknown_boots_require_recovery() {
        for policy in [
            Policy {
                user: Startup::Runtime,
                system: Startup::Disabled,
            },
            Policy {
                user: Startup::Disabled,
                system: Startup::Runtime,
            },
        ] {
            assert_eq!(policy.restore_for_boot(Some("old"), "old").unwrap(), policy);
            assert_eq!(
                policy.restore_for_boot(Some("old"), "new").unwrap(),
                Policy {
                    user: Startup::Disabled,
                    system: Startup::Disabled
                }
            );
            assert!(policy.restore_for_boot(None, "new").is_err());
        }
        let persistent = Policy {
            user: Startup::Enabled,
            system: Startup::Disabled,
        };
        assert_eq!(
            persistent.restore_for_boot(None, "new").unwrap(),
            persistent
        );
    }

    #[test]
    fn reports_require_loaded_known_units_and_disabled_global_startup() {
        let unit = |scope: ServiceScope, startup: &str| {
            serde_json::json!({
                "state": "known", "value": {
                    "name": scope.unit(), "load_state": "loaded", "active_state": "inactive",
                    "sub_state": "dead", "unit_file_state": startup, "main_pid": 0, "fragment_path": "fixture"
                }
            })
        };
        let source = serde_json::json!({
            "context": InstallationContext::Native,
            "user": unit(ServiceScope::User, "enabled-runtime"),
            "system": unit(ServiceScope::System, "disabled"),
            "global_user": { "state": "known", "value": "disabled" }
        });
        let read = |value| Policy::from_report(&serde_json::from_value(value).unwrap());
        assert_eq!(
            read(source.clone()).unwrap(),
            Policy {
                user: Startup::Runtime,
                system: Startup::Disabled
            }
        );
        for startup in [
            "masked",
            "masked-runtime",
            "linked",
            "alias",
            "static",
            "generated",
            "unknown",
        ] {
            let mut changed = source.clone();
            changed["user"] = unit(ServiceScope::User, startup);
            assert!(read(changed).is_err());
        }
        for global in ["enabled", "enabled-runtime", "not-found"] {
            let mut changed = source.clone();
            changed["global_user"]["value"] = global.into();
            assert!(read(changed).is_err());
        }
        let mut changed = source.clone();
        changed["system"]["value"]["load_state"] = "not-found".into();
        assert!(read(changed).is_err());
        let mut changed = source;
        changed["user"] = serde_json::json!({"state":"unavailable", "reason":"no manager"});
        assert!(read(changed).is_err());
    }

    #[test]
    fn startup_commands_only_modify_the_fixed_unit_enablement() {
        for scope in [ServiceScope::User, ServiceScope::System] {
            assert_eq!(
                Change::Disable { runtime: true }.arguments(scope),
                [
                    scope.argument(),
                    "--no-ask-password",
                    "--no-pager",
                    "--runtime",
                    "disable",
                    scope.unit()
                ]
            );
            assert_eq!(
                Change::Enable { runtime: false }.arguments(scope),
                [
                    scope.argument(),
                    "--no-ask-password",
                    "--no-pager",
                    "enable",
                    scope.unit()
                ]
            );
        }
    }
}
