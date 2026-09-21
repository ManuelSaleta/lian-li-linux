use crate::reservation::{HardwareReservation, ServiceOperationLock};
use crate::services::{self, Route};
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::DaemonInfo;
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{
    OwnershipSnapshot, ServiceAction, ServiceActionRequest, ServiceProbe, ServiceReport,
    ServiceScope, UnitState,
};
use std::time::{Duration, Instant};

pub fn execute(
    context: InstallationContext,
    request: ServiceActionRequest,
    progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    ensure!(
        !matches!(context, InstallationContext::UnsupportedContainer),
        "Host service actions are unavailable in this container"
    );
    let mut backend = Native {
        operation: ServiceOperationLock::acquire(&context)?,
        route: Route::detect(&context)?,
        context,
        began: Instant::now(),
    };
    if request.scope == ServiceScope::System
        && matches!(backend.context, InstallationContext::Distrobox { .. })
    {
        crate::container_change::verify_services()?;
        if request.action != ServiceAction::Stop {
            crate::lingering::require(&backend.route, unsafe { libc::geteuid() })?;
        }
    } else if request.scope == ServiceScope::System && request.action != ServiceAction::Stop {
        if let Some(deployment) = crate::container_deployment::load()? {
            crate::lingering::require(&backend.route, deployment.owner_uid)?;
        }
    }
    run(&mut backend, request, progress)
}

pub(crate) trait Backend {
    fn user_uid(&self) -> u32 {
        unsafe { libc::geteuid() }
    }
    fn inspect(&mut self) -> Result<ServiceReport>;
    fn probe(&mut self, scope: ServiceScope, owner: &OwnershipSnapshot) -> Result<DaemonInfo>;
    fn reserve_idle(&mut self, owner: &OwnershipSnapshot) -> Result<()>;
    fn dispatch(&mut self, request: ServiceActionRequest) -> Result<()>;
    fn elapsed(&self) -> Duration;
    fn pause(&mut self);
}

struct Native {
    operation: ServiceOperationLock,
    route: Route,
    context: InstallationContext,
    began: Instant,
}

impl Backend for Native {
    fn inspect(&mut self) -> Result<ServiceReport> {
        self.operation.verify()?;
        Ok(services::inspect(&self.context))
    }
    fn probe(&mut self, scope: ServiceScope, owner: &OwnershipSnapshot) -> Result<DaemonInfo> {
        crate::daemon_probe::inspect(&self.context, scope, owner)
    }
    fn reserve_idle(&mut self, owner: &OwnershipSnapshot) -> Result<()> {
        HardwareReservation::acquire(&self.context, &owner.identity)?.verify()
    }
    fn dispatch(&mut self, request: ServiceActionRequest) -> Result<()> {
        self.operation.verify()?;
        self.route.service_action(request)
    }
    fn elapsed(&self) -> Duration {
        self.began.elapsed()
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn known<T>(probe: &ServiceProbe<T>) -> Result<&T> {
    match probe {
        ServiceProbe::Known { value } => Ok(value),
        ServiceProbe::Unavailable { reason } => {
            anyhow::bail!("Service state is unverified: {reason}")
        }
    }
}

fn unit(report: &ServiceReport, scope: ServiceScope) -> Result<&UnitState> {
    known(match scope {
        ServiceScope::User => &report.user,
        ServiceScope::System => &report.system,
    })
}

fn owner(report: &ServiceReport) -> Result<&OwnershipSnapshot> {
    known(
        report
            .ownership
            .as_ref()
            .context("Kernel ownership has not been checked")?,
    )
}

fn verify_owner(snapshot: &OwnershipSnapshot, scope: ServiceScope) -> Result<()> {
    let process = known(
        snapshot
            .process
            .as_ref()
            .context("Hardware owner process is unverified")?,
    )?;
    ensure!(snapshot.owner_pid == Some(process.pid) && process.service == Some(scope),
        "The hardware owner is a manual launch, another user's daemon or another service. It will not be stopped");
    Ok(())
}

fn preflight(report: &ServiceReport, request: ServiceActionRequest, user_uid: u32) -> Result<()> {
    let target = unit(report, request.scope)?;
    if let InstallationContext::Distrobox { name } = &report.context {
        ensure!(target.distrobox_name.as_deref() == Some(name.as_str()), "Install the current Distrobox service recipe for this box with its invocation ID and guarded ExecStop command, then reload the host service manager");
    }
    ensure!(
        target.load_state == "loaded",
        "The selected service is not loaded. Install and reload its unit first"
    );
    let ownership = owner(report)?;
    match request.action {
        ServiceAction::Start => {
            ensure!(
                matches!(target.active_state.as_str(), "inactive" | "failed")
                    && target.main_pid == 0,
                "The selected service is already running or changing state"
            );
            ensure!(ownership.owner_pid.is_none(), "Another daemon still owns the hardware. Stop it cleanly before starting this service");
        }
        ServiceAction::Stop | ServiceAction::Restart => {
            ensure!(
                target.active_state == "active" && target.sub_state == "running",
                "Wait for the selected service to finish changing state"
            );
            verify_owner(ownership, request.scope)?;
        }
    }
    if request.action != ServiceAction::Stop {
        if let Some(selection) = known(
            report
                .selection
                .as_ref()
                .context("Host service selection has not been checked")?,
        )? {
            ensure!(selection.scope == request.scope
                && (request.scope != ServiceScope::User || selection.uid == user_uid),
                "Another host service mode or account is selected. Use the mode-switch operation first");
        }
        let other = unit(
            report,
            match request.scope {
                ServiceScope::User => ServiceScope::System,
                ServiceScope::System => ServiceScope::User,
            },
        )?;
        ensure!(
            matches!(other.active_state.as_str(), "inactive" | "failed") && other.main_pid == 0,
            "The other service mode is active or changing state"
        );
        ensure!(other.load_state == "not-found" || matches!(other.unit_file_state.as_str(), "disabled" | "masked" | "masked-runtime"),
            "The other hardware mode is selected for startup. Disable it or use the mode-switch operation first");
        ensure!(matches!(known(&report.global_user)?.as_str(), "disabled" | "masked" | "masked-runtime" | "not-found"),
            "Global user-service startup can launch competing daemons. Resolve it before starting or restarting a service");
    }
    Ok(())
}

pub(crate) fn run(
    backend: &mut impl Backend,
    request: ServiceActionRequest,
    mut progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    progress("Checking service state and hardware ownership…")?;
    let before = backend.inspect()?;
    preflight(&before, request, backend.user_uid())?;
    let previous_owner = owner(&before)?;
    let previous = if request.action == ServiceAction::Start {
        backend.reserve_idle(previous_owner)?;
        None
    } else {
        let info = backend
            .probe(request.scope, previous_owner)
            .context("Cannot verify the currently running service")?;
        verify_invocation(unit(&before, request.scope)?, &info)?;
        verify_write_gate(&before, &info)?;
        Some(info)
    };
    progress("Approve the service authorization prompt to continue…")?;
    backend
        .dispatch(request)
        .context("The action was not retried. Recheck service state if the request timed out")?;
    progress("Waiting for the service and verifying hardware ownership…")?;
    let deadline = backend.elapsed() + Duration::from_secs(90);
    let mut detail = String::from("The service has not reached the requested state");
    while backend.elapsed() < deadline {
        let result = (|| -> Result<bool> {
            let current = backend.inspect()?;
            let target = unit(&current, request.scope)?;
            let ownership = owner(&current)?;
            ensure!(
                ownership.identity == previous_owner.identity,
                "The hardware lock was replaced during the operation"
            );
            detail = format!(
                "{}: {}/{}",
                target.name, target.active_state, target.sub_state
            );
            if request.action == ServiceAction::Stop {
                if target.active_state == "inactive"
                    && target.main_pid == 0
                    && ownership.owner_pid.is_none()
                {
                    backend.reserve_idle(ownership)?;
                    return Ok(true);
                }
            } else if target.active_state == "active" && target.sub_state == "running" {
                verify_owner(ownership, request.scope)?;
                let info = backend.probe(request.scope, ownership)?;
                verify_invocation(target, &info)?;
                verify_write_gate(&current, &info)?;
                if previous
                    .as_ref()
                    .is_none_or(|old| old.instance_id != info.instance_id)
                {
                    return Ok(true);
                }
                detail = "The previous daemon instance is still running".into();
            }
            Ok(false)
        })();
        match result {
            Ok(true) => {
                return Ok(match request.action {
                    ServiceAction::Start => {
                        "Service started. Daemon identity and loaded settings verified."
                    }
                    ServiceAction::Stop => "Service stopped cleanly. Hardware ownership released.",
                    ServiceAction::Restart => {
                        "Service restarted. New daemon identity and loaded settings verified."
                    }
                }
                .into())
            }
            Ok(false) => {}
            Err(error) => detail = format!("{error:#}"),
        }
        backend.pause();
    }
    anyhow::bail!("Service completion could not be verified: {detail}. No forced kill or automatic retry was issued. Recheck services and inspect the selected service's journal.")
}

fn verify_invocation(unit: &UnitState, info: &DaemonInfo) -> Result<()> {
    if unit.distrobox_name.is_some() {
        ensure!(unit.invocation_id.is_some() && unit.invocation_id == info.service_invocation
            && info.capabilities.iter().any(|value| value == lianli_shared::daemon::SERVICE_STOP),
            "The container daemon does not match the host service invocation or lacks guarded stop support");
    }
    Ok(())
}

fn verify_write_gate(report: &ServiceReport, info: &DaemonInfo) -> Result<()> {
    let expected = known(
        report
            .operation_lock
            .as_ref()
            .context("Host service write lock is unverified")?,
    )?;
    ensure!(info.capabilities.iter().any(|value| value == lianli_shared::daemon::SERVICE_WRITE_GATE)
        && info.service_operation_lock.as_ref() == Some(expected),
        "The daemon cannot coordinate settings writes with this service operation. Update the daemon and repair the shared host control lock before using service controls");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::daemon::{DaemonMode, FileIdentity};
    use lianli_shared::services::OwnerProcess;

    fn report(active: bool) -> ServiceReport {
        let unit = |scope: ServiceScope, running: bool| ServiceProbe::Known {
            value: UnitState {
                name: scope.unit().into(),
                load_state: "loaded".into(),
                active_state: if running { "active" } else { "inactive" }.into(),
                sub_state: if running { "running" } else { "dead" }.into(),
                unit_file_state: "disabled".into(),
                main_pid: if running { 123 } else { 0 },
                fragment_path: String::new(),
                control_group: Some("/unit".into()),
                kill_mode: Some("mixed".into()),
                send_sigkill: Some(false),
                graceful_shutdown: Some(true),
                invocation_id: None,
                distrobox_name: None,
            },
        };
        ServiceReport {
            context: InstallationContext::Native,
            selection: Some(ServiceProbe::Known { value: None }),
            operation_lock: Some(ServiceProbe::Known {
                value: FileIdentity {
                    device: "1".into(),
                    inode: "3".into(),
                },
            }),
            user: unit(ServiceScope::User, active),
            system: unit(ServiceScope::System, false),
            global_user: ServiceProbe::Known {
                value: "disabled".into(),
            },
            ownership: Some(ServiceProbe::Known {
                value: OwnershipSnapshot {
                    identity: FileIdentity {
                        device: "1".into(),
                        inode: "2".into(),
                    },
                    owner_pid: active.then_some(123),
                    process: active.then_some(ServiceProbe::Known {
                        value: OwnerProcess {
                            pid: 123,
                            effective_uid: 1000,
                            start_time_ticks: "1".into(),
                            control_group: "/unit".into(),
                            service: Some(ServiceScope::User),
                        },
                    }),
                },
            }),
        }
    }

    struct Fixture {
        before: ServiceReport,
        after: ServiceReport,
        calls: usize,
        now: Duration,
        new_instance: bool,
        reject: bool,
    }
    impl Backend for Fixture {
        fn inspect(&mut self) -> Result<ServiceReport> {
            Ok(if self.calls == 0 {
                self.before.clone()
            } else {
                self.after.clone()
            })
        }
        fn probe(&mut self, _: ServiceScope, _: &OwnershipSnapshot) -> Result<DaemonInfo> {
            Ok(DaemonInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: 1,
                instance_id: if self.calls > 0 && self.new_instance {
                    "new"
                } else {
                    "old"
                }
                .into(),
                pid: 123,
                mode: DaemonMode::User,
                config_path: "/config.json".into(),
                capabilities: vec![lianli_shared::daemon::SERVICE_WRITE_GATE.into()],
                ownership_lock: None,
                service_invocation: None,
                service_operation_lock: Some(FileIdentity {
                    device: "1".into(),
                    inode: "3".into(),
                }),
            })
        }
        fn reserve_idle(&mut self, owner: &OwnershipSnapshot) -> Result<()> {
            ensure!(owner.owner_pid.is_none());
            Ok(())
        }
        fn dispatch(&mut self, _: ServiceActionRequest) -> Result<()> {
            self.calls += 1;
            ensure!(!self.reject, "Authorization cancelled");
            Ok(())
        }
        fn elapsed(&self) -> Duration {
            self.now
        }
        fn pause(&mut self) {
            self.now += Duration::from_secs(1);
        }
    }
    fn fixture(before: bool, after: bool) -> Fixture {
        Fixture {
            before: report(before),
            after: report(after),
            calls: 0,
            now: Duration::ZERO,
            new_instance: true,
            reject: false,
        }
    }
    fn request(action: ServiceAction) -> ServiceActionRequest {
        ServiceActionRequest {
            scope: ServiceScope::User,
            action,
        }
    }

    #[test]
    fn service_controls_respect_the_hosts_shutdown_policy() {
        for distrobox in [false, true] {
            for mode in ["mixed", "control-group", "process", "none"] {
                for send_sigkill in [false, true] {
                    let mut snapshot = report(false);
                    let ServiceProbe::Known { value } = &mut snapshot.user else {
                        unreachable!()
                    };
                    value.distrobox_name = distrobox.then(|| "test-box".into());
                    value.kill_mode = Some(mode.into());
                    value.send_sigkill = Some(send_sigkill);
                    value.graceful_shutdown = Some(false);
                    assert!(
                        preflight(&snapshot, request(ServiceAction::Start), 1000).is_ok(),
                        "box={distrobox}, mode={mode}, forced={send_sigkill}"
                    );
                }
            }
        }
    }

    #[test]
    fn service_controls_require_the_daemons_write_gate_to_match_the_host_lock() {
        let mut backend = fixture(true, false);
        let ownership = owner(&backend.before).unwrap().clone();
        let mut info = backend.probe(ServiceScope::User, &ownership).unwrap();
        assert!(verify_write_gate(&backend.before, &info).is_ok());
        info.service_operation_lock.as_mut().unwrap().inode = "other".into();
        assert!(verify_write_gate(&backend.before, &info).is_err());
        info.service_operation_lock = None;
        assert!(verify_write_gate(&backend.before, &info).is_err());
        info = backend.probe(ServiceScope::User, &ownership).unwrap();
        info.capabilities.clear();
        assert!(verify_write_gate(&backend.before, &info).is_err());
    }

    #[test]
    fn authorized_preflight_matches_the_caller_instead_of_the_root_coordinator() {
        let mut report = report(false);
        report.selection = Some(ServiceProbe::Known {
            value: Some(lianli_shared::services::ServiceSelection {
                scope: ServiceScope::User,
                uid: 1000,
            }),
        });
        let request = ServiceActionRequest {
            scope: ServiceScope::User,
            action: ServiceAction::Start,
        };
        assert!(preflight(&report, request, 1000).is_ok());
        assert!(preflight(&report, request, 0).is_err());
        assert!(preflight(&report, request, 1001).is_err());
    }

    #[test]
    fn host_selection_blocks_conflicting_starts_but_allows_a_verified_stop() {
        let mut report = report(true);
        report.selection = Some(ServiceProbe::Known {
            value: Some(lianli_shared::services::ServiceSelection {
                scope: ServiceScope::System,
                uid: 999,
            }),
        });
        let request = ServiceActionRequest {
            scope: ServiceScope::User,
            action: ServiceAction::Restart,
        };
        assert!(preflight(&report, request, unsafe { libc::geteuid() }).is_err());
        assert!(preflight(
            &report,
            ServiceActionRequest {
                action: ServiceAction::Stop,
                ..request
            },
            unsafe { libc::geteuid() }
        )
        .is_ok());
        report.selection = Some(ServiceProbe::Unavailable {
            reason: "Hidden host selection".into(),
        });
        assert!(preflight(&report, request, unsafe { libc::geteuid() }).is_err());
        assert!(preflight(
            &report,
            ServiceActionRequest {
                action: ServiceAction::Stop,
                ..request
            },
            unsafe { libc::geteuid() }
        )
        .is_ok());
        report.selection = Some(ServiceProbe::Known {
            value: Some(lianli_shared::services::ServiceSelection {
                scope: ServiceScope::User,
                uid: unsafe { libc::geteuid() }.wrapping_add(1),
            }),
        });
        assert!(preflight(&report, request, unsafe { libc::geteuid() }).is_err());
    }

    #[test]
    fn container_actions_require_the_exact_box_for_both_service_modes() {
        for scope in [ServiceScope::User, ServiceScope::System] {
            let mut report = report(false);
            report.context = InstallationContext::Distrobox {
                name: "fixture-box".into(),
            };
            let request = ServiceActionRequest {
                scope,
                action: ServiceAction::Start,
            };
            for name in [Some("fixture-box"), Some("other-box"), None] {
                let target = match scope {
                    ServiceScope::User => &mut report.user,
                    ServiceScope::System => &mut report.system,
                };
                let ServiceProbe::Known { value } = target else {
                    unreachable!()
                };
                value.distrobox_name = name.map(str::to_owned);
                value.kill_mode = Some("control-group".into());
                assert_eq!(
                    preflight(&report, request, 1000).is_ok(),
                    name == Some("fixture-box")
                );
            }
        }
    }

    #[test]
    fn old_distrobox_recipe_is_rejected_even_if_its_owner_matches_the_cgroup() {
        let mut backend = fixture(true, false);
        backend.before.context = InstallationContext::Distrobox { name: "box".into() };
        assert!(run(&mut backend, request(ServiceAction::Stop), |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("Distrobox service recipe"));
        assert_eq!(backend.calls, 0);
    }

    #[test]
    fn failed_progress_publication_does_not_dispatch_or_repeat_service_actions() {
        for failure_at in [1, 2, 3] {
            let mut backend = fixture(true, true);
            let mut count = 0;
            let result = run(&mut backend, request(ServiceAction::Restart), |_| {
                count += 1;
                ensure!(count != failure_at, "Progress storage unavailable");
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(backend.calls, usize::from(failure_at == 3));
        }
    }

    #[test]
    fn start_stop_and_restart_require_their_observed_outcomes() {
        for (action, before, after) in [
            (ServiceAction::Start, false, true),
            (ServiceAction::Stop, true, false),
            (ServiceAction::Restart, true, true),
        ] {
            let mut backend = fixture(before, after);
            assert!(run(&mut backend, request(action), |_| Ok(())).is_ok());
            assert_eq!(backend.calls, 1);
        }
        let mut unchanged = fixture(true, true);
        unchanged.new_instance = false;
        assert!(run(&mut unchanged, request(ServiceAction::Restart), |_| Ok(())).is_err());
        assert_eq!(unchanged.calls, 1);
        let mut held = fixture(true, true);
        assert!(run(&mut held, request(ServiceAction::Stop), |_| Ok(())).is_err());
        assert_eq!(held.calls, 1);
    }

    #[test]
    fn cancellation_and_preflight_failures_never_retry_or_stop_an_unrelated_owner() {
        let mut cancelled = fixture(true, true);
        cancelled.reject = true;
        assert!(run(&mut cancelled, request(ServiceAction::Restart), |_| Ok(())).is_err());
        assert_eq!(cancelled.calls, 1);
        let mut manual = fixture(true, false);
        if let Some(ServiceProbe::Known { value }) = &mut manual.before.ownership {
            if let Some(ServiceProbe::Known { value }) = &mut value.process {
                value.service = None;
            }
        }
        assert!(run(&mut manual, request(ServiceAction::Stop), |_| Ok(())).is_err());
        assert_eq!(manual.calls, 0);
        let mut conflict = fixture(false, true);
        conflict.before.global_user = ServiceProbe::Known {
            value: "enabled".into(),
        };
        assert!(run(&mut conflict, request(ServiceAction::Start), |_| Ok(())).is_err());
        assert_eq!(conflict.calls, 0);
    }
}
