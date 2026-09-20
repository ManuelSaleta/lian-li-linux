use crate::services::Route;
use anyhow::{ensure, Result};
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationFinding, InstallationGuide,
};
use lianli_shared::services::{
    ServiceProbe, ServiceReport, ServiceScope, ServiceSelection, UnitState,
};

pub fn guidance(uid: u32) -> String {
    format!("On the host, run sudo loginctl enable-linger {uid}, then Recheck. Lingering allows this account's user services to run before login and after logout. Lian Li Linux does not change it automatically.")
}

fn parse(value: &str) -> Result<bool> {
    match value.trim() {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => anyhow::bail!("The host did not report a valid lingering setting"),
    }
}

fn inspect(route: &Route, uid: u32) -> Result<bool> {
    let output = route.output(
        "/usr/bin/loginctl",
        &[
            "--no-pager",
            "--no-ask-password",
            "show-user",
            &uid.to_string(),
            "--property=Linger",
            "--value",
        ],
    )?;
    ensure!(
        output.status.success(),
        "Cannot check host user lingering: {}",
        output.stderr.trim()
    );
    parse(&output.stdout)
}

pub(crate) fn require(route: &Route, uid: u32) -> Result<()> {
    let enabled =
        inspect(route, uid).map_err(|error| anyhow::anyhow!("{error:#}. {}", guidance(uid)))?;
    ensure!(
        enabled,
        "Distrobox system mode requires user lingering. {}",
        guidance(uid)
    );
    Ok(())
}

pub fn finding(report: &ServiceReport) -> Option<InstallationFinding> {
    let ServiceProbe::Known { value } = &report.system else {
        return None;
    };
    value.distrobox_name.as_ref()?;
    let mut owner_uid = None;
    let result = Route::detect(&report.context).and_then(|route| {
        let uid = crate::distrobox_service::system_owner_uid(&route)?;
        owner_uid = Some(uid);
        inspect(&route, uid)
    });
    Some(checked_finding(
        system_mode_required(value, report.selection.as_ref()),
        owner_uid,
        result,
    ))
}

fn system_mode_required(
    unit: &UnitState,
    selection: Option<&ServiceProbe<Option<ServiceSelection>>>,
) -> bool {
    !(crate::native_switch::idle(unit)
        && matches!(
            unit.unit_file_state.as_str(),
            "disabled" | "masked" | "masked-runtime"
        )
        && matches!(
            selection,
            Some(ServiceProbe::Known {
                value: None
                    | Some(ServiceSelection {
                        scope: ServiceScope::User,
                        ..
                    })
            })
        ))
}

fn checked_finding(
    required: bool,
    owner_uid: Option<u32>,
    result: Result<bool>,
) -> InstallationFinding {
    let (mut state, mut evidence) = match result {
        Ok(true) => (CheckState::Passed, "Host user lingering is enabled.".into()),
        Ok(false) => (
            CheckState::Failed,
            "Without lingering, the boxed system daemon stops after logout.".into(),
        ),
        Err(error) => (CheckState::Unavailable, format!("{error:#}")),
    };
    if !required && state != CheckState::Passed {
        state = CheckState::NotApplicable;
        evidence = format!("Lingering is only required for Distrobox system mode. The system service is unused; no change is needed for user mode. {evidence}");
    }
    InstallationFinding {
        code: "services.distrobox_lingering".into(),
        state,
        severity: if matches!(state, CheckState::Passed | CheckState::NotApplicable) {
            FindingSeverity::Info
        } else {
            FindingSeverity::Warning
        },
        feature: "Services".into(),
        context: owner_uid.map_or_else(
            || "Host account".into(),
            |uid| format!("Host account {uid}"),
        ),
        title: "Distrobox system startup".into(),
        evidence,
        remediation: owner_uid.map_or_else(
            || {
                "Verify the host system unit's account, then follow the Distrobox lingering guide."
                    .into()
            },
            guidance,
        ),
        guide: InstallationGuide::Distrobox,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_system_mode_does_not_raise_a_lingering_warning() {
        let mut unit: UnitState = serde_json::from_value(serde_json::json!({
            "name": "lianli-daemon-system.service", "load_state": "loaded",
            "active_state": "inactive", "sub_state": "dead", "unit_file_state": "disabled",
            "main_pid": 0, "fragment_path": "/etc/systemd/system/lianli-daemon-system.service"
        }))
        .unwrap();
        let user = ServiceProbe::Known {
            value: Some(ServiceSelection {
                scope: ServiceScope::User,
                uid: 1000,
            }),
        };
        let system = ServiceProbe::Known {
            value: Some(ServiceSelection {
                scope: ServiceScope::System,
                uid: 1000,
            }),
        };
        assert!(!system_mode_required(&unit, Some(&user)));
        for result in [Ok(false), Err(anyhow::anyhow!("host query unavailable"))] {
            let finding =
                checked_finding(system_mode_required(&unit, Some(&user)), Some(1000), result);
            assert_eq!(finding.state, CheckState::NotApplicable);
            assert_eq!(finding.severity, FindingSeverity::Info);
        }
        assert!(system_mode_required(&unit, Some(&system)));
        assert!(system_mode_required(&unit, None));
        assert!(system_mode_required(
            &unit,
            Some(&ServiceProbe::Unavailable {
                reason: "unknown".into()
            })
        ));
        unit.unit_file_state = "enabled".into();
        assert!(system_mode_required(&unit, Some(&user)));
        unit.unit_file_state = "disabled".into();
        unit.active_state = "active".into();
        assert!(system_mode_required(&unit, Some(&user)));
        let failed = checked_finding(true, Some(1000), Ok(false));
        assert_eq!(failed.state, CheckState::Failed);
        assert_eq!(failed.severity, FindingSeverity::Warning);
        assert_eq!(
            checked_finding(true, Some(1000), Err(anyhow::anyhow!("unavailable"))).state,
            CheckState::Unavailable
        );
    }

    #[test]
    fn accepts_only_explicit_host_lingering_state() {
        assert!(super::parse("yes\n").unwrap());
        assert!(!super::parse("no\n").unwrap());
        for value in ["", "1", "true", "Linger=yes", "yes\nno", "unknown"] {
            assert!(super::parse(value).is_err(), "{value:?}");
        }
    }
}
