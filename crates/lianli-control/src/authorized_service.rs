use crate::account::Account;
use crate::reservation::{HardwareReservation, ServiceOperationLock};
use crate::service_operation::Backend;
use crate::services::Route;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::DaemonInfo;
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{
    OwnershipSnapshot, ServiceActionRequest, ServiceProbe, ServiceReport, ServiceScope,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
pub struct Observation {
    owner: OwnershipSnapshot,
    info: DaemonInfo,
}

pub fn observe(scope: ServiceScope) -> Result<Observation> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native
            && unsafe { libc::geteuid() } != 0,
        "Observe native services under the caller's unprivileged account"
    );
    let report = crate::services::inspect(&InstallationContext::Native);
    let owner = match report.ownership {
        Some(ServiceProbe::Known { value }) => value,
        _ => anyhow::bail!("The hardware owner could not be verified"),
    };
    let info = crate::daemon_probe::inspect(&InstallationContext::Native, scope, &owner)?;
    Ok(Observation { owner, info })
}

pub fn execute(
    caller: &Account,
    operation: &ServiceOperationLock,
    request: ServiceActionRequest,
    progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    let mut backend = Authorized::new(caller, operation)?;
    if request.scope == ServiceScope::System
        && request.action != lianli_shared::services::ServiceAction::Stop
    {
        if let Some(deployment) = crate::container_deployment::load()? {
            deployment.verify_owner(caller)?;
            crate::lingering::require(&Route::Native, caller.uid)?;
        }
    }
    crate::service_operation::run(&mut backend, request, progress)
}

pub(crate) struct Authorized<'a> {
    caller: &'a Account,
    operation: &'a ServiceOperationLock,
    began: Instant,
}

impl<'a> Authorized<'a> {
    pub(crate) fn new(caller: &'a Account, operation: &'a ServiceOperationLock) -> Result<Self> {
        ensure!(
            unsafe { libc::geteuid() } == 0
                && InstallationContext::detect() == InstallationContext::Native,
            "Service coordination requires its authorized native root process"
        );
        ensure!(
            std::env::current_exe()?.file_name() == Some(OsStr::new("lianli-control")),
            "Use the standalone coordinator for account service actions"
        );
        ensure!(
            &Account::user(caller.uid)? == caller,
            "The caller's account or groups changed after switch preflight"
        );
        operation.verify()?;
        Ok(Self {
            caller,
            operation,
            began: Instant::now(),
        })
    }

    fn helper<T: serde::de::DeserializeOwned>(&self, args: &[&OsStr]) -> Result<T> {
        self.operation.verify()?;
        let output =
            crate::command::run(self.caller.control_command(args)?, Duration::from_secs(60))?;
        self.operation.verify()?;
        ensure!(
            output.status.success(),
            "Caller-account service inspection failed: {}",
            output.stderr.trim()
        );
        serde_json::from_str(&output.stdout).context("Invalid caller-account service inspection")
    }
}

impl Backend for Authorized<'_> {
    fn user_uid(&self) -> u32 {
        self.caller.uid
    }

    fn inspect(&mut self) -> Result<ServiceReport> {
        let report: ServiceReport = self.helper(&[OsStr::new("diagnose")])?;
        ensure!(
            report.context == InstallationContext::Native,
            "Service inspection left the native host"
        );
        Ok(report)
    }

    fn probe(&mut self, scope: ServiceScope, owner: &OwnershipSnapshot) -> Result<DaemonInfo> {
        let observation: Observation = self.helper(&[
            OsStr::new("observe-service"),
            OsStr::new("--scope"),
            OsStr::new(match scope {
                ServiceScope::User => "user",
                ServiceScope::System => "system",
            }),
        ])?;
        observation.validate(owner, scope, self.caller.uid)?;
        Ok(observation.info)
    }

    fn reserve_idle(&mut self, owner: &OwnershipSnapshot) -> Result<()> {
        self.operation.verify()?;
        HardwareReservation::acquire(&InstallationContext::Native, &owner.identity)?.verify()
    }

    fn dispatch(&mut self, request: ServiceActionRequest) -> Result<()> {
        self.operation.verify()?;
        match request.scope {
            ServiceScope::System => Route::Native.service_action(request)?,
            ServiceScope::User => {
                let output = crate::command::run(
                    self.caller.user_service_command(request.action)?,
                    Duration::from_secs(30),
                )?;
                ensure!(
                    output.status.success(),
                    "User service request failed: {}",
                    output.stderr.trim()
                );
            }
        }
        self.operation.verify()
    }

    fn elapsed(&self) -> Duration {
        self.began.elapsed()
    }

    fn pause(&mut self) {
        std::thread::sleep(Duration::from_secs(1));
    }
}

impl Observation {
    fn validate(
        &self,
        previous: &OwnershipSnapshot,
        scope: ServiceScope,
        user_uid: u32,
    ) -> Result<()> {
        let process = |snapshot: &OwnershipSnapshot| match &snapshot.process {
            Some(ServiceProbe::Known { value }) => Ok(value.clone()),
            _ => anyhow::bail!("The service owner process is unverified"),
        };
        let old = process(previous)?;
        let current = process(&self.owner)?;
        ensure!(
            self.owner.identity == previous.identity
                && self.owner.owner_pid == previous.owner_pid
                && self.owner.owner_pid == Some(current.pid)
                && old.pid == current.pid
                && old.effective_uid == current.effective_uid
                && old.start_time_ticks == current.start_time_ticks
                && old.control_group == current.control_group
                && old.service == current.service
                && current.service == Some(scope)
                && (scope != ServiceScope::User || current.effective_uid == user_uid),
            "The caller-account service owner changed during inspection"
        );
        self.info
            .write_guard(env!("CARGO_PKG_VERSION"))
            .map_err(anyhow::Error::msg)?;
        ensure!(
            self.info.pid == current.pid
                && self.info.ownership_lock.as_ref() == Some(&previous.identity),
            "The daemon identity differs from the inspected native hardware owner"
        );
        ensure!(
            self.info.mode
                == match scope {
                    ServiceScope::User => lianli_shared::daemon::DaemonMode::User,
                    ServiceScope::System => lianli_shared::daemon::DaemonMode::System,
                },
            "The daemon uses a different configuration mode"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::daemon::{DaemonMode, FileIdentity, GRACEFUL_SHUTDOWN, GUARDED_WRITES};
    use lianli_shared::services::OwnerProcess;

    fn observation() -> Observation {
        let identity = FileIdentity {
            device: "1".into(),
            inode: "2".into(),
        };
        Observation {
            owner: OwnershipSnapshot {
                identity: identity.clone(),
                owner_pid: Some(123),
                process: Some(ServiceProbe::Known {
                    value: OwnerProcess {
                        pid: 123,
                        effective_uid: 1000,
                        start_time_ticks: "99".into(),
                        control_group: "/user.slice/fixture.service".into(),
                        service: Some(ServiceScope::User),
                    },
                }),
            },
            info: DaemonInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: 1,
                instance_id: "fixture".into(),
                pid: 123,
                mode: DaemonMode::User,
                config_path: "/home/fixture/config.json".into(),
                capabilities: vec![GUARDED_WRITES.into(), GRACEFUL_SHUTDOWN.into()],
                ownership_lock: Some(identity),
                service_invocation: None,
                service_operation_lock: None,
            },
        }
    }

    #[test]
    fn account_observation_requires_the_same_process_account_mode_and_lock() {
        let original = observation();
        assert!(original
            .validate(&original.owner, ServiceScope::User, 1000)
            .is_ok());
        assert!(original
            .validate(&original.owner, ServiceScope::User, 1001)
            .is_err());
        assert!(original
            .validate(&original.owner, ServiceScope::System, 1000)
            .is_err());
        for change in 0..10 {
            let mut changed = observation();
            match change {
                0 => changed.owner.identity.inode = "different".into(),
                1 => changed.owner.owner_pid = Some(124),
                2 => changed.info.pid = 124,
                3 => changed.info.mode = DaemonMode::System,
                4 => changed.info.ownership_lock = None,
                5 => changed.info.capabilities.clear(),
                _ => {
                    let Some(ServiceProbe::Known { value }) = &mut changed.owner.process else {
                        unreachable!()
                    };
                    match change {
                        6 => value.start_time_ticks = "100".into(),
                        7 => value.control_group = "/another.service".into(),
                        8 => value.effective_uid = 1001,
                        _ => value.service = None,
                    }
                }
            }
            assert!(
                changed
                    .validate(&original.owner, ServiceScope::User, 1000)
                    .is_err(),
                "change {change}"
            );
        }
    }

    #[test]
    fn system_observation_uses_its_service_account_and_rejects_unknown_owners() {
        let mut current = observation();
        current.info.mode = DaemonMode::System;
        let Some(ServiceProbe::Known { value }) = &mut current.owner.process else {
            unreachable!()
        };
        value.effective_uid = 900;
        value.service = Some(ServiceScope::System);
        value.control_group = "/system.slice/lianli-daemon-system.service".into();
        assert!(current
            .validate(&current.owner, ServiceScope::System, 1000)
            .is_ok());
        let mut unknown = current.owner.clone();
        unknown.process = None;
        assert!(current
            .validate(&unknown, ServiceScope::System, 1000)
            .is_err());
    }
}
