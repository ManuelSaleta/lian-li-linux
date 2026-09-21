use super::*;

const UNIT: &str = "lianli-session.service";

pub fn check_distrobox_desktop_startup(name: &str) -> InstallationFinding {
    let inspect = || -> Result<(UnitState, Option<String>)> {
        let route = Route::detect(&InstallationContext::Distrobox { name: name.into() })?;
        let output = route.query(&["--user", "show", "--all", "--property", PROPERTIES, UNIT])?;
        ensure!(output.status.success(), "{}", command_error(&output));
        let state = parse_unit(&output.stdout, UNIT)?;
        let recipe = if state.load_state == "loaded" {
            crate::distrobox_service::inspect_session(&route)?
        } else {
            None
        };
        let state = parse_unit_with_stop(&output.stdout, UNIT, recipe.is_some())?;
        Ok((state, recipe))
    };
    finding(name, inspect())
}

fn finding(name: &str, inspected: Result<(UnitState, Option<String>)>) -> InstallationFinding {
    let (state, title, evidence) = match inspected {
        Err(error) => (
            CheckState::Unavailable,
            "Desktop login startup could not be inspected",
            format!("Host service inspection failed: {error:#}"),
        ),
        Ok((unit, recipe)) => {
            let detail = format!(
                "{UNIT}: load {}, startup {}, runtime {}/{}.",
                unit.load_state, unit.unit_file_state, unit.active_state, unit.sub_state
            );
            if unit.load_state == "not-found" {
                (
                    CheckState::NotApplicable,
                    "Optional desktop login service is not installed",
                    detail,
                )
            } else if unit.load_state == "loaded"
                && unit.unit_file_state == "disabled"
                && unit.active_state == "inactive"
            {
                (
                    CheckState::NotApplicable,
                    "Optional desktop login startup is disabled",
                    detail,
                )
            } else if unit.load_state != "loaded" {
                (
                    CheckState::Failed,
                    "Desktop login service is missing or unavailable",
                    detail,
                )
            } else if recipe.as_deref() != Some(name) {
                (CheckState::Failed, "Desktop login service needs the current recipe",
                    format!("{detail} Start and guarded stop commands must identify the same capture service invocation in box '{name}'."))
            } else if unit.unit_file_state != "enabled" {
                (
                    CheckState::Failed,
                    "Desktop login service is not enabled persistently",
                    detail,
                )
            } else if unit.active_state == "failed" {
                (CheckState::Failed, "Desktop login service failed", detail)
            } else {
                (CheckState::Passed, "Desktop login startup is configured",
                    format!("{detail} The host service targets box '{name}' and is enabled for future logins. Runtime state does not establish successful capture or panel playback."))
            }
        }
    };
    InstallationFinding {
        code: "desktop.login_startup".into(),
        state,
        severity: if matches!(state, CheckState::Passed | CheckState::NotApplicable) { FindingSeverity::Info } else { FindingSeverity::Warning },
        feature: "Desktop display login startup".into(),
        context: "Host user service. Capture inside Distrobox".into(),
        title: title.into(),
        evidence,
        remediation: format!("{} Inspect host logs with journalctl --user -u {UNIT} -b -n 100 --no-pager. This check does not start or change services.",
            crate::distrobox_unit::session_guidance(name).unwrap_or_else(|error| format!("{error:#}. Follow the Distrobox desktop startup guide."))),
        guide: InstallationGuide::Distrobox,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> UnitState {
        parse_unit_with_stop(
            "Id=lianli-session.service\nLoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nMainPID=10\nKillMode=control-group\nSendSIGKILL=no\nKillSignal=15\nRestartKillSignal=15\nTimeoutStopFailureMode=terminate\n",
            UNIT, true,
        ).unwrap()
    }

    #[test]
    fn version_1_0_capture_startup_passes_without_claiming_playback() {
        for runtime in ["active", "inactive", "activating"] {
            let mut unit = configured();
            unit.active_state = runtime.into();
            let result = finding("box", Ok((unit, Some("box".into()))));
            assert_eq!(result.state, CheckState::Passed);
            assert!(result
                .evidence
                .contains("does not establish successful capture"));
        }
    }

    #[test]
    fn configured_capture_respects_distribution_shutdown_overrides() {
        let mut unit = configured();
        unit.graceful_shutdown = Some(false);
        unit.kill_mode = Some("mixed".into());
        unit.send_sigkill = Some(true);
        let result = finding("box", Ok((unit, Some("box".into()))));
        assert_eq!(result.state, CheckState::Passed);
    }

    #[test]
    fn missing_disabled_failed_and_wrong_box_services_need_attention() {
        for (field, value) in [
            ("load", "masked"),
            ("startup", "disabled"),
            ("startup", "enabled-runtime"),
            ("runtime", "failed"),
        ] {
            let mut unit = configured();
            match field {
                "load" => unit.load_state = value.into(),
                "startup" => unit.unit_file_state = value.into(),
                "runtime" => unit.active_state = value.into(),
                _ => unreachable!(),
            }
            assert_eq!(
                finding("box", Ok((unit, Some("box".into())))).state,
                CheckState::Failed
            );
        }
        for recipe in [None, Some("other-box".into())] {
            assert_eq!(
                finding("box", Ok((configured(), recipe))).state,
                CheckState::Failed
            );
        }
    }

    #[test]
    fn optional_capture_can_remain_uninstalled_or_disabled() {
        for installed in [false, true] {
            let mut unit = configured();
            unit.load_state = if installed { "loaded" } else { "not-found" }.into();
            unit.unit_file_state = "disabled".into();
            unit.active_state = "inactive".into();
            let result = finding("box", Ok((unit, None)));
            assert_eq!(result.state, CheckState::NotApplicable);
            assert_eq!(result.severity, FindingSeverity::Info);
        }
    }

    #[test]
    fn inaccessible_host_remains_unverified_with_the_reason() {
        let result = finding("box", Err(anyhow::anyhow!("host bus unavailable")));
        assert_eq!(result.state, CheckState::Unavailable);
        assert!(result.evidence.contains("host bus unavailable"));
    }
}
