use crate::account::Account;
use crate::native_switch::{check_launch, idle, known, ownership, unit};
use crate::reservation::HardwareReservation;
use crate::services::Route;
use crate::switch_journal::{Journal, Phase, Record};
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::{DaemonInfo, FileIdentity};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceAction, ServiceActionRequest, ServiceReport, ServiceScope};
use std::time::{Duration, Instant};

pub(crate) fn eligible(record: &Record) -> bool {
    record.intent.source.scope == ServiceScope::System
        && !matches!(
            record.phase,
            Phase::Preparing | Phase::Ready | Phase::Complete | Phase::RolledBack
        )
}

fn verify_selection(report: &ServiceReport, record: &Record) -> Result<()> {
    ensure!(
        known(
            report
                .selection
                .as_ref()
                .context("Host selection is unverified")?
        )? == &Some(record.intent.source),
        "Keep the user destination blocked until recovery finishes"
    );
    Ok(())
}

fn validate_running(report: &ServiceReport, record: &Record, info: &DaemonInfo) -> Result<()> {
    verify_selection(report, record)?;
    validate_source(report, record, info)
}

fn validate_source(report: &ServiceReport, record: &Record, info: &DaemonInfo) -> Result<()> {
    ensure!(
        known(&report.global_user)? == "disabled",
        "Global user startup changed. Preserve its configuration and recheck recovery"
    );
    let target = unit(report, ServiceScope::System)?;
    let owner = ownership(report)?;
    let process = known(
        owner
            .process
            .as_ref()
            .context("System owner is unverified")?,
    )?;
    ensure!(
        target.active_state == "active"
            && target.sub_state == "running"
            && owner.owner_pid == Some(process.pid)
            && target.main_pid == process.pid
            && process.service == Some(ServiceScope::System)
            && process.effective_uid == record.intent.source.uid,
        "The system hardware owner differs from the recorded source"
    );
    info.write_guard(env!("CARGO_PKG_VERSION"))
        .map_err(anyhow::Error::msg)?;
    ensure!(
        info.pid == process.pid
            && info.config_path == record.intent.source_config
            && info.ownership_lock.as_ref() == Some(&owner.identity)
            && info.mode == lianli_shared::daemon::DaemonMode::System,
        "The system daemon loaded a different configuration or hardware identity"
    );
    for capability in [
        lianli_shared::daemon::SERVICE_STARTUP_GATE,
        lianli_shared::daemon::SERVICE_WRITE_GATE,
        lianli_shared::daemon::GRACEFUL_SHUTDOWN,
    ] {
        ensure!(
            info.capabilities.iter().any(|value| value == capability),
            "The system source lacks recovery capability {capability}"
        );
    }
    ensure!(
        info.service_operation_lock.as_ref()
            == Some(known(
                report
                    .operation_lock
                    .as_ref()
                    .context("Operation lock is unverified")?
            )?),
        "The system daemon uses a different operation lock"
    );
    Ok(())
}

fn verify_running(report: &ServiceReport, record: &Record, expected: &FileIdentity) -> Result<()> {
    verify_source(report, record, expected, validate_running)
}

pub(crate) fn verify_live_source(record: &Record, expected: &FileIdentity) -> Result<()> {
    let report = crate::services::inspect(&InstallationContext::Native);
    let (startup, should_run) = record.system_restoration(&crate::service_startup::boot_id()?)?;
    ensure!(
        should_run,
        "The original startup policy does not permit a running system source"
    );
    verify_source(&report, record, expected, |report, record, info| {
        ensure!(
            crate::service_startup::read_startup(&report.system)? == startup,
            "The running system source has a different startup policy"
        );
        validate_source(report, record, info)
    })
}

fn verify_source(
    report: &ServiceReport,
    record: &Record,
    expected: &FileIdentity,
    validate: impl Fn(&ServiceReport, &Record, &DaemonInfo) -> Result<()>,
) -> Result<()> {
    let before = ownership(report)?;
    ensure!(
        &before.identity == expected,
        "The hardware lock changed during system recovery"
    );
    let info =
        crate::daemon_probe::inspect(&InstallationContext::Native, ServiceScope::System, before)?;
    validate(report, record, &info)?;
    let after = crate::services::inspect(&InstallationContext::Native);
    validate(&after, record, &info)?;
    ensure!(
        known(
            ownership(&after)?
                .process
                .as_ref()
                .context("System owner disappeared")?
        )? == known(
            before
                .process
                .as_ref()
                .context("System owner is unverified")?
        )?,
        "The system process changed during verification"
    );
    Ok(())
}

pub(crate) fn resume(
    journal: &mut Journal<'_>,
    system: &Account,
    expected: &FileIdentity,
) -> Result<()> {
    ensure!(
        eligible(journal.record())
            && unsafe { libc::geteuid() } == 0
            && InstallationContext::detect() == InstallationContext::Native,
        "Offline recovery requires the native system source journal"
    );
    let boot = crate::service_startup::boot_id()?;
    let (startup, should_run) = journal.record().system_restoration(&boot)?;
    ensure!(
        system.uid == journal.record().intent.source.uid,
        "The system account changed"
    );
    check_launch(
        system,
        ServiceScope::System,
        &journal.record().intent.source_config,
        journal
            .record()
            .intent
            .source_working_directory
            .as_deref()
            .context("The original system working directory is unknown")?,
    )?;
    journal.advance(Phase::RestoringSystem)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut submitted = false;
    let mut detail = String::from("The system service is still changing state");
    loop {
        let report = crate::services::inspect(&InstallationContext::Native);
        let target = unit(&report, ServiceScope::System)?;
        let owner = ownership(&report)?;
        ensure!(
            &owner.identity == expected,
            "The hardware lock changed during system restoration"
        );
        if target.active_state == "active" && target.sub_state == "running" {
            ensure!(
                should_run,
                "The system service is running unexpectedly. Preserve it and inspect recovery"
            );
            ensure!(
                crate::service_startup::read_startup(&report.system)? == startup,
                "The running system service has a different startup policy"
            );
            let verified = (|| {
                journal.resume_live_system(expected)?;
                verify_running(
                    &crate::services::inspect(&InstallationContext::Native),
                    journal.record(),
                    expected,
                )
            })();
            match verified {
                Ok(()) => return journal.advance(Phase::SystemRestored),
                Err(error) => detail = format!("{error:#}"),
            }
        }
        if idle(target) && !submitted {
            ensure!(owner.owner_pid.is_none(), "Another daemon still owns the hardware. It will not be stopped by offline recovery");
            ensure!(
                known(&report.global_user)? == "disabled",
                "Global user startup must remain disabled during recovery"
            );
            journal.pause_launches()?;
            let hardware = HardwareReservation::acquire(&InstallationContext::Native, expected)?;
            journal.restore_system_selection(&hardware)?;
            if !should_run {
                return journal.advance(Phase::SystemRestored);
            }
            journal.submit_system_start(&boot)?;
            drop(hardware);
            Route::Native.service_action(ServiceActionRequest { scope: ServiceScope::System, action: ServiceAction::Start })
                .context("System recovery startup was not confirmed. Inspect its journal without replaying the request")?;
            submitted = true;
        }
        ensure!(Instant::now() < deadline, "System restoration is still unverified: {detail}. Preserve the journal and inspect the system service. No forced stop or repeated start was issued");
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (ServiceReport, Record, DaemonInfo) {
        let identity = json!({"device":"1", "inode":"2"});
        let operation = json!({"device":"1", "inode":"3"});
        let selection = json!({"scope":"system", "uid":2000});
        let record = serde_json::from_value(json!({
            "version":1, "boot_id":"123456789abcdef0123456789abcdef0",
            "phase":"restoring_system", "prepared":null, "destination_backup":null,
            "rollback_backup":null, "destination_instance":null, "failure":null,
            "intent": {
                "id":"abcdef0123456789abcdef0123456789", "caller_name":"fixture",
                "source":selection, "destination":{"scope":"user", "uid":1000},
                "source_config":"/var/lib/lianli/config.json", "destination_config":"/home/fixture/config.json",
                "source_was_running":true, "previous_selection":selection,
                "user_startup":"disabled", "system_startup":"enabled", "carry_settings":false
            }
        })).unwrap();
        let report = serde_json::from_value(json!({
            "context":InstallationContext::Native,
            "selection":{"state":"known", "value":selection},
            "user":{"state":"unavailable", "reason":"No user manager before login"},
            "system":{"state":"known", "value":{
                "name":ServiceScope::System.unit(), "load_state":"loaded", "active_state":"active",
                "sub_state":"running", "unit_file_state":"enabled", "main_pid":123, "fragment_path":"fixture"
            }},
            "global_user":{"state":"known", "value":"disabled"},
            "ownership":{"state":"known", "value":{
                "identity":identity, "owner_pid":123, "process":{"state":"known", "value":{
                    "pid":123, "effective_uid":2000, "start_time_ticks":"10",
                    "control_group":"/system.slice/lianli-daemon-system.service", "service":"system"
                }}
            }},
            "operation_lock":{"state":"known", "value":operation}
        })).unwrap();
        let info = serde_json::from_value(json!({
            "version":env!("CARGO_PKG_VERSION"), "protocol_version":1, "instance_id":"fixture",
            "pid":123, "mode":"system", "config_path":"/var/lib/lianli/config.json",
            "ownership_lock":identity, "service_operation_lock":operation,
            "capabilities":[lianli_shared::daemon::SERVICE_STARTUP_GATE, lianli_shared::daemon::SERVICE_WRITE_GATE,
                lianli_shared::daemon::GRACEFUL_SHUTDOWN, lianli_shared::daemon::GUARDED_WRITES]
        })).unwrap();
        (report, record, info)
    }

    #[test]
    fn a_paused_gate_does_not_hide_the_live_sources_identity() {
        let (mut report, record, mut info) = fixture();
        report.selection = Some(lianli_shared::services::ServiceProbe::Unavailable {
            reason: "Startup paused".into(),
        });
        assert!(validate_running(&report, &record, &info).is_err());
        validate_source(&report, &record, &info).unwrap();
        info.pid += 1;
        assert!(validate_source(&report, &record, &info).is_err());
    }

    #[test]
    fn system_verification_does_not_require_a_user_manager_but_checks_owner_and_selection() {
        let (report, record, info) = fixture();
        validate_running(&report, &record, &info).unwrap();
        for field in [
            "user",
            "system",
            "global_user",
            "ownership",
            "operation_lock",
            "selection",
        ] {
            let mut changed = serde_json::to_value(&report).unwrap();
            changed[field] = json!({"state":"unavailable", "reason":"fixture unavailable"});
            let changed = serde_json::from_value(changed).unwrap();
            assert_eq!(
                validate_running(&changed, &record, &info).is_ok(),
                field == "user",
                "{field}"
            );
        }
        for replacement in [
            json!(null),
            json!({"scope":"user", "uid":1000}),
            json!({"scope":"system", "uid":2001}),
        ] {
            let mut changed = serde_json::to_value(&report).unwrap();
            changed["selection"]["value"] = replacement;
            assert!(
                validate_running(&serde_json::from_value(changed).unwrap(), &record, &info)
                    .is_err()
            );
        }
        for field in ["pid", "effective_uid", "service"] {
            let mut changed = serde_json::to_value(&report).unwrap();
            changed["ownership"]["value"]["process"]["value"][field] = match field {
                "service" => json!("user"),
                _ => json!(9000),
            };
            let changed = serde_json::from_value(changed).unwrap();
            assert!(validate_running(&changed, &record, &info).is_err());
        }
        let mut changed = info.clone();
        changed.config_path = "/different/config.json".into();
        assert!(validate_running(&report, &record, &changed).is_err());
        changed = info.clone();
        changed.capabilities.clear();
        assert!(validate_running(&report, &record, &changed).is_err());
        changed = info;
        changed.service_operation_lock = None;
        assert!(validate_running(&report, &record, &changed).is_err());
    }
}
