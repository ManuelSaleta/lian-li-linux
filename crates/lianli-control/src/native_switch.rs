use crate::account::Account;
use crate::authorized_service::Authorized;
use crate::destination::Destination;
use crate::reservation::{HardwareReservation, ServiceOperationLock};
use crate::service_operation::Backend;
use crate::service_startup::Policy;
use crate::switch_journal::{Intent, Journal, Phase};
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::{DaemonInfo, FileIdentity};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{
    OwnershipSnapshot, ServiceAction, ServiceActionRequest, ServiceProbe, ServiceReport,
    ServiceScope, ServiceSelection, UnitState,
};
use std::io::Read;
use std::path::Path;

fn other(scope: ServiceScope) -> ServiceScope {
    match scope {
        ServiceScope::User => ServiceScope::System,
        ServiceScope::System => ServiceScope::User,
    }
}

pub(crate) fn known<T>(value: &ServiceProbe<T>) -> Result<&T> {
    match value {
        ServiceProbe::Known { value } => Ok(value),
        ServiceProbe::Unavailable { reason } => {
            anyhow::bail!("Service state is unverified: {reason}")
        }
    }
}

pub(crate) fn unit(report: &ServiceReport, scope: ServiceScope) -> Result<&UnitState> {
    known(match scope {
        ServiceScope::User => &report.user,
        ServiceScope::System => &report.system,
    })
}

pub(crate) fn ownership(report: &ServiceReport) -> Result<&OwnershipSnapshot> {
    known(
        report
            .ownership
            .as_ref()
            .context("Hardware ownership is unverified")?,
    )
}

pub(crate) fn idle(unit: &UnitState) -> bool {
    matches!(unit.active_state.as_str(), "inactive" | "failed") && unit.main_pid == 0
}

fn initial_state(
    report: &ServiceReport,
    destination: ServiceScope,
    source_uid: u32,
) -> Result<(Policy, bool)> {
    let policy = Policy::from_report(report)?;
    policy.validate()?;
    ensure!(
        idle(unit(report, destination)?),
        "The destination service is running or changing state"
    );
    let source = unit(report, other(destination))?;
    let owner = ownership(report)?;
    if idle(source) {
        ensure!(
            owner.owner_pid.is_none(),
            "A manual or foreign daemon owns the hardware. Stop it cleanly before switching"
        );
        return Ok((policy, false));
    }
    ensure!(
        source.active_state == "active" && source.sub_state == "running",
        "Wait for the source service to finish changing state"
    );
    let process = known(
        owner
            .process
            .as_ref()
            .context("The source process is unverified")?,
    )?;
    ensure!(
        owner.owner_pid == Some(process.pid)
            && process.service == Some(other(destination))
            && process.effective_uid == source_uid,
        "The source hardware owner belongs to another account or launch route"
    );
    Ok((policy, true))
}

fn verify_daemon(
    backend: &mut impl Backend,
    scope: ServiceScope,
    config: &Path,
    uid: u32,
) -> Result<DaemonInfo> {
    let report = backend.inspect()?;
    ensure!(
        unit(&report, scope)?.active_state == "active"
            && unit(&report, scope)?.sub_state == "running",
        "The selected daemon is not running"
    );
    ensure!(
        idle(unit(&report, other(scope))?),
        "The other hardware mode is active or changing state"
    );
    let owner = ownership(&report)?;
    let process = known(
        owner
            .process
            .as_ref()
            .context("The selected daemon owner is unverified")?,
    )?;
    ensure!(
        process.effective_uid == uid && process.service == Some(scope),
        "The selected daemon uses a different account or service"
    );
    let info = backend.probe(scope, owner)?;
    ensure!(
        info.config_path == config,
        "The daemon loaded a different configuration path"
    );
    ensure!(
        info.capabilities
            .iter()
            .any(|value| value == lianli_shared::daemon::SERVICE_STARTUP_GATE),
        "Restart the installed daemon with startup-gate support before switching"
    );
    Ok(info)
}

fn action(
    backend: &mut impl Backend,
    scope: ServiceScope,
    action: ServiceAction,
    progress: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    crate::service_operation::run(backend, ServiceActionRequest { scope, action }, progress)?;
    Ok(())
}

fn start(
    backend: &mut impl Backend,
    selection: ServiceSelection,
    config: &Path,
    progress: &mut impl FnMut(&str) -> Result<()>,
    mut verify_context: impl FnMut() -> Result<()>,
) -> Result<DaemonInfo> {
    let deadline = backend.elapsed() + std::time::Duration::from_secs(90);
    loop {
        verify_context()?;
        let report = backend.inspect()?;
        let target = unit(&report, selection.scope)?;
        if idle(target) {
            verify_context()?;
            action(backend, selection.scope, ServiceAction::Start, progress)?;
            return verify_daemon(backend, selection.scope, config, selection.uid);
        }
        let detail = if target.active_state == "active" && target.sub_state == "running" {
            match verify_daemon(backend, selection.scope, config, selection.uid) {
                Ok(info) => return Ok(info),
                Err(error) => format!("{error:#}"),
            }
        } else {
            format!(
                "{}: {}/{}",
                target.name, target.active_state, target.sub_state
            )
        };
        ensure!(backend.elapsed() < deadline,
            "The selected service did not finish startup: {detail}. Recovery remains pending without a forced restart");
        backend.pause();
    }
}

fn stop_destination(
    backend: &mut Authorized<'_>,
    scope: ServiceScope,
    progress: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let deadline = backend.elapsed() + std::time::Duration::from_secs(90);
    loop {
        let report = backend.inspect()?;
        let target = unit(&report, scope)?;
        if idle(target) {
            return Ok(());
        }
        if target.active_state == "active" && target.sub_state == "running" {
            return action(backend, scope, ServiceAction::Stop, progress);
        }
        ensure!(backend.elapsed() < deadline,
            "Destination shutdown or startup is still in progress. Recovery remains pending without a forced stop");
        backend.pause();
    }
}

fn stop_recovered_system(
    backend: &mut Authorized<'_>,
    record: &crate::switch_journal::Record,
    progress: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let deadline = backend.elapsed() + std::time::Duration::from_secs(90);
    loop {
        let report = backend.inspect()?;
        let target = unit(&report, ServiceScope::System)?;
        if idle(target) {
            return Ok(());
        }
        if target.active_state == "active" && target.sub_state == "running" {
            verify_daemon(
                backend,
                ServiceScope::System,
                &record.intent.source_config,
                record.intent.source.uid,
            )?;
            return action(backend, ServiceScope::System, ServiceAction::Stop, progress);
        }
        ensure!(
            backend.elapsed() < deadline,
            "The recovered system source is still changing state. It will not be forced to stop"
        );
        backend.pause();
    }
}

fn reserve(backend: &mut Authorized<'_>, expected: &FileIdentity) -> Result<HardwareReservation> {
    let report = backend.inspect()?;
    ensure!(
        idle(unit(&report, ServiceScope::User)?) && idle(unit(&report, ServiceScope::System)?),
        "Wait for both service processes to exit cleanly before publication or recovery"
    );
    let owner = ownership(&report)?;
    ensure!(
        owner.owner_pid.is_none() && &owner.identity == expected,
        "Hardware ownership changed during the switch"
    );
    HardwareReservation::acquire(&InstallationContext::Native, expected)
}

fn same_state(
    account: &Account,
    destination: &Destination,
    expected: &crate::saved_state::Checked,
) -> Result<()> {
    ensure!(&crate::saved_state::check(account, destination, false)? == expected,
        "Settings or media changed during switch preparation. Restore the previous mode and prepare again");
    Ok(())
}

pub(crate) fn check_launch(
    account: &Account,
    scope: ServiceScope,
    config: &Path,
    working: &Path,
) -> Result<()> {
    let current = crate::destination::preflight_existing(account, scope)?;
    ensure!(
        current.config_path == config && current.working_directory == working,
        "The destination launch environment changed. Restore the previous mode before retrying"
    );
    Ok(())
}

pub fn execute(
    destination_scope: ServiceScope,
    carry: bool,
    progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    execute_for(
        Account::authorized_caller()?,
        destination_scope,
        carry,
        progress,
    )
}

pub(crate) fn execute_for(
    caller: Account,
    destination_scope: ServiceScope,
    carry: bool,
    mut progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Automatic switching requires the native host application"
    );
    let system = Account::system()?;
    let operation = ServiceOperationLock::acquire(&InstallationContext::Native)?;
    ensure!(
        Journal::load(&operation)?.is_none(),
        "Recover the pending switch before starting another"
    );
    let mut backend = Authorized::new(&caller, &operation)?;
    let (source, destination) = if destination_scope == ServiceScope::User {
        (&system, &caller)
    } else {
        (&caller, &system)
    };
    progress("Checking both service accounts, startup selection and saved settings…")?;
    let user_runtime = crate::user_runtime::identity(caller.uid)?
        .context("Log in with a private user runtime directory before switching services")?;
    let before = backend.inspect()?;
    let (startup, source_running) = initial_state(&before, destination_scope, source.uid)?;
    let expected_lock = ownership(&before)?.identity.clone();
    let previous = crate::service_selection::inspect(&InstallationContext::Native)?;
    ensure!(!source_running || previous.is_none_or(|selection| selection == ServiceSelection { scope: other(destination_scope), uid: source.uid }),
        "The running source conflicts with the selected host account. Resolve that selection before a recoverable switch");
    let source_state = crate::destination::preflight(source, other(destination_scope))?;
    let destination_state = crate::destination::preflight(destination, destination_scope)?;
    if carry && destination_state.assets.failed > 0 {
        progress("Old destination media is unavailable. Its settings will be backed up and replaced by the copied settings.")?;
    }
    if source_running {
        verify_daemon(
            &mut backend,
            other(destination_scope),
            &source_state.config_path,
            source.uid,
        )?;
    }
    ensure!(
        !carry || source_state.state.is_some(),
        "Save current settings before carrying them to another service account"
    );
    let source_checked = crate::saved_state::check(source, &source_state, false)?;
    let destination_checked = crate::saved_state::check(destination, &destination_state, !carry)?;
    let mut random = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut journal = Journal::create(
        &operation,
        Intent {
            id,
            caller_name: caller.name.clone(),
            source: ServiceSelection {
                scope: other(destination_scope),
                uid: source.uid,
            },
            destination: ServiceSelection {
                scope: destination_scope,
                uid: destination.uid,
            },
            source_config: source_state.config_path.clone(),
            destination_config: destination_state.config_path.clone(),
            source_working_directory: Some(source_state.working_directory.clone()),
            destination_working_directory: Some(destination_state.working_directory.clone()),
            source_was_running: source_running,
            previous_selection: previous,
            user_startup: startup.user,
            system_startup: startup.system,
            carry_settings: carry,
            user_runtime: Some(user_runtime.clone()),
        },
    )?;
    let result = (|| {
        if carry {
            progress(
                "Preparing settings and decoding copied media under the destination account…",
            )?;
            let prepared = crate::state_transfer::prepare(
                source,
                &source_state,
                destination,
                &destination_state,
                &journal.record().intent.id,
                &operation,
            )?;
            journal.prepared(prepared)?;
        }
        same_state(source, &source_state, &source_checked)?;
        same_state(destination, &destination_state, &destination_checked)?;
        let current = backend.inspect()?;
        let (current_startup, current_running) =
            initial_state(&current, destination_scope, source.uid)?;
        ensure!(
            current_startup == startup
                && current_running == source_running
                && ownership(&current)?.identity == expected_lock,
            "Service ownership or startup selection changed during preparation"
        );
        journal.advance(Phase::Ready)?;
        ensure!(
            crate::user_runtime::identity(caller.uid)?.as_ref() == Some(&user_runtime),
            "The desktop runtime changed during preparation. Prepare the switch again"
        );
        ensure!(
            Account::user(caller.uid)? == caller && Account::system()? == system,
            "A service account changed during preparation. Prepare the switch again"
        );
        check_launch(
            source,
            other(destination_scope),
            &source_state.config_path,
            &source_state.working_directory,
        )?;
        check_launch(
            destination,
            destination_scope,
            &destination_state.config_path,
            &destination_state.working_directory,
        )?;
        progress("Pausing competing starts and stopping the current daemon cleanly…")?;
        journal.advance(Phase::StoppingSource)?;
        journal.pause_launches()?;
        if source_running {
            action(
                &mut backend,
                other(destination_scope),
                ServiceAction::Stop,
                &mut progress,
            )?;
        }
        journal.advance(Phase::SourceStopped)?;
        let hardware = reserve(&mut backend, &expected_lock)?;
        same_state(source, &source_state, &source_checked)?;
        same_state(destination, &destination_state, &destination_checked)?;
        if carry {
            progress("Publishing prepared settings and media with rollback backups…")?;
            journal.publish_destination(destination, &hardware)?;
        } else {
            journal.advance(Phase::Published)?;
        }
        journal.advance(Phase::SelectingDestination)?;
        check_launch(
            destination,
            destination_scope,
            &destination_state.config_path,
            &destination_state.working_directory,
        )?;
        journal.select_destination(&caller, &hardware)?;
        journal.advance(Phase::StartingDestination)?;
        drop(hardware);
        progress("Starting and verifying the selected daemon…")?;
        let info = start(
            &mut backend,
            journal.record().intent.destination,
            &destination_state.config_path,
            &mut progress,
            || Ok(()),
        )?;
        journal.advance(Phase::VerifyingDestination)?;
        journal.verified_destination(&info.instance_id)?;
        journal.advance(Phase::Complete)
    })();
    match result {
        Ok(()) => {
            journal.cleanup_preparation(destination)?;
            journal.finish()?;
            Ok("Service mode switched. Selected account and loaded configuration have been verified.".into())
        }
        Err(error) => {
            journal.failure(&format!("{error:#}"))?;
            if let Err(recovery) = rollback(
                &mut journal,
                &mut backend,
                &caller,
                source,
                destination,
                &expected_lock,
                &mut progress,
            ) {
                journal.failure(&format!(
                    "Switch failed: {error:#}. Recovery also failed: {recovery:#}"
                ))?;
                if crate::system_recovery::eligible(journal.record()) {
                    let resumed =
                        crate::system_recovery::resume(&mut journal, &system, &expected_lock);
                    let detail = match resumed {
                        Ok(()) => "System startup restored. User settings recovery will resume when the session and home are available".into(),
                        Err(error) => format!("System restoration is also pending: {error:#}"),
                    };
                    journal.failure(&format!("{error:#}. {recovery:#}. {detail}"))?;
                    return Err(error).context(detail);
                }
                return Err(error).context(format!("Recovery remains pending: {recovery:#}. Preserve the switch journal and recover before trying another switch"));
            }
            journal.cleanup_preparation(destination)?;
            journal.finish()?;
            Err(error).context("The switch failed. Previous service mode was restored")
        }
    }
}

fn rollback(
    journal: &mut Journal<'_>,
    backend: &mut Authorized<'_>,
    caller: &Account,
    source: &Account,
    destination: &Account,
    expected_lock: &FileIdentity,
    progress: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut report = |message: &str| {
        // Progress storage failure must not prevent restoration after the source stops.
        let _ = progress(message);
        Ok(())
    };
    let progress = &mut report;
    if journal.record().phase == Phase::RestoringSystem {
        crate::system_recovery::resume(journal, source, expected_lock)?;
    }
    if matches!(
        journal.record().phase,
        Phase::SystemRestored | Phase::StoppingRecoveredSystem
    ) {
        let report = backend.inspect()?;
        Policy::from_report(&report)?;
        ensure!(
            idle(unit(&report, ServiceScope::User)?),
            "Wait for the blocked user destination to become inactive before finishing recovery"
        );
        crate::destination::preflight_recovery(
            destination,
            &journal.record().intent.destination_config,
        )?;
        check_launch(
            source,
            ServiceScope::System,
            &journal.record().intent.source_config,
            journal
                .record()
                .intent
                .source_working_directory
                .as_deref()
                .context("The original system working directory is unknown")?,
        )?;
        if journal.record().phase == Phase::SystemRestored {
            journal.advance(Phase::StoppingRecoveredSystem)?;
        }
        journal.pause_launches()?;
        stop_recovered_system(backend, journal.record(), progress)?;
        let hardware = reserve(backend, expected_lock)?;
        journal.recovered_system_stopped(&hardware)?;
    }
    if matches!(journal.record().phase, Phase::Preparing | Phase::Ready) {
        if journal.record().intent.carry_settings {
            crate::state_transfer::discard_as(
                destination,
                &journal.record().intent.destination_config,
                &journal.record().intent.id,
            )?;
        }
        return journal.advance(Phase::RolledBack);
    }
    let recovery_runtime = journal.current_user_runtime()?;
    let source_should_run = journal.source_should_run()?;
    if !matches!(
        journal.record().phase,
        Phase::StoppingDestination
            | Phase::RestoringDestination
            | Phase::RestoringStartup
            | Phase::StartingSource
            | Phase::VerifyingSource
    ) {
        journal.advance(Phase::StoppingDestination)?;
    }
    progress("Restoring the previous service mode…")?;
    if journal.record().phase == Phase::StoppingDestination {
        unit(
            &backend.inspect()?,
            journal.record().intent.destination.scope,
        )?;
        journal.pause_launches()?;
        stop_destination(backend, journal.record().intent.destination.scope, progress)?;
        let hardware = reserve(backend, expected_lock)?;
        journal.restore_destination(destination, &hardware)?;
    }
    if journal.record().phase == Phase::RestoringDestination {
        let hardware = reserve(backend, expected_lock)?;
        journal.restore_destination(destination, &hardware)?;
    }
    if journal.record().phase == Phase::RestoringStartup {
        let report = backend.inspect()?;
        if source_should_run && !idle(unit(&report, journal.record().intent.source.scope)?) {
            ensure!(
                Policy::from_report(&report)? == journal.restoration_policy()?
                    && crate::service_selection::inspect(&InstallationContext::Native)?
                        == journal.record().intent.previous_selection,
                "The previous daemon restarted before its startup policy was restored"
            );
            check_launch(source, journal.record().intent.source.scope,
                &journal.record().intent.source_config,
                journal.record().intent.source_working_directory.as_deref().context("The original working directory is unknown. Preserve the journal for manual recovery")?)?;
            start(
                backend,
                journal.record().intent.source,
                &journal.record().intent.source_config,
                progress,
                || check_recovery_runtime(journal, &recovery_runtime),
            )?;
        } else {
            let hardware = reserve(backend, expected_lock)?;
            check_launch(source, journal.record().intent.source.scope,
                &journal.record().intent.source_config,
                journal.record().intent.source_working_directory.as_deref().context("The original working directory is unknown. Preserve the journal for manual recovery")?)?;
            journal.restore_selection(caller, &hardware)?;
        }
        journal.advance(Phase::StartingSource)?;
    }
    if journal.record().phase == Phase::StartingSource {
        ensure!(
            journal.current_user_runtime()? == recovery_runtime,
            "The user runtime changed during restoration. Resume recovery in the current session"
        );
        ensure!(Policy::from_report(&backend.inspect()?)? == journal.restoration_policy()?
            && crate::service_selection::inspect(&InstallationContext::Native)? == journal.record().intent.previous_selection,
            "Startup selection changed before source restart. Preserve the journal and recheck recovery");
        if source_should_run {
            start(
                backend,
                journal.record().intent.source,
                &journal.record().intent.source_config,
                progress,
                || check_recovery_runtime(journal, &recovery_runtime),
            )?;
        } else {
            reserve(backend, expected_lock)?;
        }
        journal.advance(Phase::VerifyingSource)?;
    }
    if journal.record().phase == Phase::VerifyingSource {
        ensure!(journal.current_user_runtime()? == recovery_runtime,
            "The user runtime changed during source startup. Preserve the journal and recheck recovery");
        if source_should_run {
            verify_daemon(
                backend,
                journal.record().intent.source.scope,
                &journal.record().intent.source_config,
                journal.record().intent.source.uid,
            )?;
        } else {
            reserve(backend, expected_lock)?;
        }
        journal.advance(Phase::RolledBack)?;
    }
    Ok(())
}

pub fn recover(progress: impl FnMut(&str) -> Result<()>) -> Result<String> {
    recover_for(Account::authorized_caller()?, progress)
}

fn check_recovery_runtime(journal: &Journal<'_>, expected: &Option<FileIdentity>) -> Result<()> {
    ensure!(&journal.current_user_runtime()? == expected,
        "The user runtime changed while waiting for startup. Resume recovery in the current session");
    Ok(())
}

pub(crate) fn recover_for(
    caller: Account,
    progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    recover_matching(caller, None, progress)
}

pub(crate) fn recover_matching(
    caller: Account,
    expected_id: Option<&str>,
    mut progress: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Recover service switching from the native host application"
    );
    let system = Account::system()?;
    let operation = ServiceOperationLock::acquire(&InstallationContext::Native)?;
    let Some(mut journal) = Journal::load(&operation)? else {
        return Ok("No service switch needs recovery.".into());
    };
    ensure!(
        expected_id.is_none_or(|id| journal.record().intent.id == id),
        "A different switch replaced the automatic recovery request. Recheck its state"
    );
    let user = if journal.record().intent.source.scope == ServiceScope::User {
        journal.record().intent.source
    } else {
        journal.record().intent.destination
    };
    let system_selection = if user == journal.record().intent.source {
        journal.record().intent.destination
    } else {
        journal.record().intent.source
    };
    ensure!(caller.uid == user.uid && caller.name == journal.record().intent.caller_name && system.uid == system_selection.uid,
        "The journal belongs to another caller or the packaged account changed. Preserve it for administrator recovery");
    if matches!(journal.record().phase, Phase::Complete | Phase::RolledBack) {
        let destination = if journal.record().intent.destination.scope == ServiceScope::User {
            &caller
        } else {
            &system
        };
        journal.cleanup_preparation(destination)?;
        journal.finish()?;
        return Ok("Completed switch record archived.".into());
    }
    let mut backend = Authorized::new(&caller, &operation)?;
    let expected = crate::ownership::inspect(
        &InstallationContext::Native,
        &crate::services::Route::Native,
    )?
    .identity;
    let destination = if journal.record().intent.destination.scope == ServiceScope::User {
        &caller
    } else {
        &system
    };
    let source = if journal.record().intent.source.scope == ServiceScope::User {
        &caller
    } else {
        &system
    };
    if let Err(error) = rollback(
        &mut journal,
        &mut backend,
        &caller,
        source,
        destination,
        &expected,
        &mut progress,
    ) {
        journal.failure(&format!("Recovery remains pending: {error:#}"))?;
        if crate::system_recovery::eligible(journal.record()) {
            let detail = match crate::system_recovery::resume(&mut journal, &system, &expected) {
                Ok(()) => "System startup restored. User settings recovery will resume when the session and home are available".into(),
                Err(recovery) => format!("System restoration is also pending: {recovery:#}"),
            };
            journal.failure(&format!("{error:#}. {detail}"))?;
            return Err(error).context(detail);
        }
        return Err(error);
    }
    journal.cleanup_preparation(destination)?;
    journal.finish()?;
    Ok("Previous service mode restored and verified.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn logout_during_startup_wait_prevents_starting_in_a_replaced_session() {
        use std::cell::Cell;
        use std::time::Duration;
        struct Waiting<'a>(&'a Cell<bool>);
        impl Backend for Waiting<'_> {
            fn inspect(&mut self) -> Result<ServiceReport> {
                let mut state = report(false);
                if !self.0.get() {
                    state["user"]["value"]["active_state"] = json!("activating");
                    state["user"]["value"]["sub_state"] = json!("start");
                }
                Ok(serde_json::from_value(state)?)
            }
            fn probe(&mut self, _: ServiceScope, _: &OwnershipSnapshot) -> Result<DaemonInfo> {
                panic!("No daemon is ready during the fixture transition")
            }
            fn reserve_idle(&mut self, _: &OwnershipSnapshot) -> Result<()> {
                panic!("A replaced session must not reserve hardware")
            }
            fn dispatch(&mut self, _: ServiceActionRequest) -> Result<()> {
                panic!("A replaced session must not start a service")
            }
            fn elapsed(&self) -> Duration {
                Duration::from_secs(u64::from(self.0.get()))
            }
            fn pause(&mut self) {
                self.0.set(true);
            }
        }
        let logged_out = Cell::new(false);
        let error = start(
            &mut Waiting(&logged_out),
            ServiceSelection {
                scope: ServiceScope::User,
                uid: 1000,
            },
            Path::new("/fixture/config.json"),
            &mut |_| Ok(()),
            || {
                ensure!(!logged_out.get(), "Runtime replaced after logout");
                Ok(())
            },
        )
        .unwrap_err();
        assert!(logged_out.get());
        assert!(error.to_string().contains("Runtime replaced after logout"));
    }

    fn report(running: bool) -> serde_json::Value {
        let unit = |scope: ServiceScope, active: bool| {
            json!({"state":"known", "value": {
                "name":scope.unit(), "load_state":"loaded", "active_state":if active {"active"} else {"inactive"},
                "sub_state":if active {"running"} else {"dead"}, "unit_file_state":if active {"enabled"} else {"disabled"},
                "main_pid":if active {123} else {0}, "fragment_path":"fixture"
            }})
        };
        json!({
            "context":InstallationContext::Native,
            "user":unit(ServiceScope::User, running), "system":unit(ServiceScope::System, false),
            "global_user":{"state":"known","value":"disabled"},
            "ownership":{"state":"known","value":{
                "identity":{"device":"1","inode":"2"}, "owner_pid":if running {Some(123)} else {None},
                "process":if running { Some(json!({"state":"known","value":{
                    "pid":123,"effective_uid":1000,"start_time_ticks":"7","control_group":"/fixture","service":"user"
                }})) } else { None }
            }}
        })
    }

    #[test]
    fn initial_inspection_distinguishes_owned_running_and_idle_sources() {
        for running in [false, true] {
            let report = serde_json::from_value(report(running)).unwrap();
            assert_eq!(
                initial_state(&report, ServiceScope::System, 1000)
                    .unwrap()
                    .1,
                running
            );
        }
        let report = serde_json::from_value(report(true)).unwrap();
        assert!(initial_state(&report, ServiceScope::System, 1001).is_err());
    }

    #[test]
    fn source_preflight_rejects_competing_unverified_or_changing_owners_before_mutation() {
        for case in 0..6 {
            let mut report = report(true);
            match case {
                0 => {
                    report["ownership"]["value"]["process"]["value"]["service"] =
                        serde_json::Value::Null
                }
                1 => report["ownership"]["value"]["owner_pid"] = 124.into(),
                2 => report["system"]["value"]["active_state"] = "activating".into(),
                3 => report["user"]["value"]["active_state"] = "deactivating".into(),
                4 => {
                    report["ownership"] =
                        json!({"state":"unavailable", "reason":"unverified kernel owner"})
                }
                _ => report["system"]["value"]["unit_file_state"] = "enabled".into(),
            }
            assert!(
                initial_state(
                    &serde_json::from_value(report).unwrap(),
                    ServiceScope::System,
                    1000
                )
                .is_err(),
                "case {case}"
            );
        }
        let mut report = report(false);
        report["ownership"]["value"]["owner_pid"] = 900.into();
        assert!(initial_state(
            &serde_json::from_value(report).unwrap(),
            ServiceScope::System,
            1000
        )
        .is_err());
    }
}
