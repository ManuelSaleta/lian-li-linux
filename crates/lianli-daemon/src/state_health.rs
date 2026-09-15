use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationFinding, InstallationGuide,
};

#[derive(Default)]
pub struct StateHealth {
    config: Option<InstallationFinding>,
    templates: Option<InstallationFinding>,
    wireless: Option<InstallationFinding>,
    device_open_errors: std::collections::BTreeMap<String, String>,
    device_open_overflow: bool,
    lcd_errors: std::collections::BTreeMap<String, String>,
    lcd_overflow: bool,
}

impl StateHealth {
    pub fn lcd_initialization(&mut self, id: &str, error: Option<&str>) {
        let Some(error) = error else {
            self.lcd_errors.remove(id);
            return;
        };
        if self.lcd_errors.len() >= 32 && !self.lcd_errors.contains_key(id) {
            self.lcd_overflow = true;
            return;
        }
        self.lcd_errors.insert(id.into(), bounded(error, 2048));
    }

    pub fn retain_lcds(&mut self, present: &std::collections::HashSet<String>) {
        self.lcd_errors.retain(|id, _| present.contains(id));
        self.lcd_overflow = false;
    }

    pub fn wireless_unavailable(&mut self, error: &str) {
        self.wireless = Some(InstallationFinding {
            code: "device.wireless.initialization".into(),
            state: CheckState::Failed,
            severity: FindingSeverity::Warning,
            feature: "Wireless control".into(),
            context: "Selected daemon".into(),
            title: "Wireless initialization failed".into(),
            evidence: bounded(error, 2048),
            remediation: "Check the transmitter connection, daemon logs and USB permissions. The daemon retries disconnected transmitters automatically. Recheck only reads status.".into(),
            guide: InstallationGuide::Troubleshooting,
        });
    }

    pub fn wireless_ready(&mut self) {
        self.wireless = None;
    }

    pub fn device_open_failed(&mut self, id: &str, error: &str) {
        if self.device_open_errors.len() >= 64 && !self.device_open_errors.contains_key(id) {
            self.device_open_overflow = true;
            return;
        }
        self.device_open_errors
            .insert(id.into(), error.chars().take(2048).collect());
    }

    pub fn device_opened(&mut self, id: &str) {
        self.device_open_errors.remove(id);
    }

    pub fn retain_devices(&mut self, present: &std::collections::HashSet<String>) {
        self.device_open_errors.retain(|id, _| present.contains(id));
        self.device_open_overflow = false;
    }

    pub fn config_loaded(&mut self, warnings: &[String], applied: bool) {
        let mut finding = loaded("configuration.load", "Configuration load", warnings);
        if !applied {
            finding.state = CheckState::Unavailable;
            finding.severity = FindingSeverity::Warning;
            finding.evidence.push_str(" Application is deferred until pixel cleaner stops. Previous settings remain active.");
        }
        self.config = Some(finding);
    }

    pub fn config_failed(&mut self, error: &str, retained: bool) {
        let mut finding = failed("configuration.load", "Configuration load", error);
        finding.evidence.push_str(if retained {
            " The last successfully loaded configuration remains active."
        } else {
            " No configuration has loaded successfully in this daemon process."
        });
        self.config = Some(finding);
    }

    pub fn templates_loaded(&mut self, warnings: &[String]) {
        self.templates = Some(loaded(
            "configuration.templates",
            "Custom template load",
            warnings,
        ));
    }

    pub fn templates_failed(&mut self, error: &str) {
        self.templates = Some(failed(
            "configuration.templates",
            "Custom template load",
            error,
        ));
    }

    pub fn findings(&self) -> Vec<InstallationFinding> {
        let mut findings: Vec<_> = self
            .config
            .iter()
            .chain(self.templates.iter())
            .chain(self.wireless.iter())
            .cloned()
            .collect();
        findings.extend(self.device_open_errors.iter().map(|(id, error)| InstallationFinding {
            code: format!("device.open.{id}"), state: CheckState::Failed, severity: FindingSeverity::Error,
            feature: "Device initialization".into(), context: id.clone(), title: "Device could not open".into(),
            evidence: error.clone(), remediation: "Check the connection, USB permissions and daemon logs. After repairs, reconnect the device or restart its service if retries have stopped. Recheck reads status only.".into(),
            guide: InstallationGuide::Troubleshooting,
        }));
        if self.device_open_overflow {
            findings.push(InstallationFinding {
                code: "device.open.more".into(),
                state: CheckState::Failed,
                severity: FindingSeverity::Error,
                feature: "Device initialization".into(),
                context: "Selected daemon".into(),
                title: "Additional device open failures".into(),
                evidence: "Only the first 64 device failures are retained.".into(),
                remediation: "Inspect daemon logs for the remaining failures.".into(),
                guide: InstallationGuide::Troubleshooting,
            });
        }
        findings.extend(self.lcd_errors.iter().map(|(id, error)| InstallationFinding {
            code: format!("device.lcd.initialization.{id}"),
            state: CheckState::Failed, severity: FindingSeverity::Warning,
            feature: "LCD initialization".into(), context: id.clone(),
            title: "LCD initialization failed".into(), evidence: error.clone(),
            remediation: "Check USB access and daemon logs. Reconnect the LCD or restart the service after repairs. Recheck only reads status.".into(),
            guide: InstallationGuide::Troubleshooting,
        }));
        if self.lcd_overflow {
            findings.push(InstallationFinding {
                code: "device.lcd.initialization.more".into(),
                state: CheckState::Failed,
                severity: FindingSeverity::Warning,
                feature: "LCD initialization".into(),
                context: "Selected daemon".into(),
                title: "Additional LCD initialization failures".into(),
                evidence: "Only the first 32 LCD initialization failures are retained.".into(),
                remediation: "Inspect daemon logs for the remaining failures.".into(),
                guide: InstallationGuide::Troubleshooting,
            });
        }
        findings
    }
}

fn base(code: &str, title: &str) -> InstallationFinding {
    InstallationFinding {
        code: code.into(), state: CheckState::Passed, severity: FindingSeverity::Info,
        feature: "Saved configuration".into(), context: "Daemon's last load attempt".into(),
        title: title.into(), evidence: String::new(),
        remediation: "Repair the settings or restore a backup, then save or restart the service. Recheck shows the last load result.".into(),
        guide: InstallationGuide::Troubleshooting,
    }
}

fn loaded(code: &str, title: &str, warnings: &[String]) -> InstallationFinding {
    let mut finding = base(code, title);
    if warnings.is_empty() {
        finding.evidence = "The last load completed without validation warnings.".into();
    } else {
        finding.state = CheckState::Failed;
        finding.severity = FindingSeverity::Warning;
        let details = warnings
            .iter()
            .take(8)
            .map(|warning| bounded(warning, 400))
            .collect::<Vec<_>>()
            .join("\n");
        finding.evidence = format!("{} load/validation warning(s): {details}", warnings.len());
    }
    finding
}

fn failed(code: &str, title: &str, error: &str) -> InstallationFinding {
    let mut finding = base(code, title);
    finding.state = CheckState::Failed;
    finding.severity = FindingSeverity::Error;
    finding.evidence = bounded(error, 3200);
    finding
}

fn bounded(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcd_failures_are_bounded_and_clear_without_losing_registry_failures() {
        let mut health = StateHealth::default();
        health.device_open_failed("hid:shared", "Registry failed");
        for id in 0..40 {
            health.lcd_initialization(&format!("lcd:{id:02}"), Some(&"é".repeat(3000)));
        }
        let findings = health.findings();
        assert_eq!(findings.len(), 34);
        assert_eq!(findings[1].evidence.chars().count(), 2048);
        assert_eq!(
            findings.last().unwrap().code,
            "device.lcd.initialization.more"
        );
        health.lcd_initialization("lcd:00", Some("Replacement failure"));
        assert_eq!(health.findings()[1].evidence, "Replacement failure");
        health.lcd_initialization("lcd:00", None);
        health.retain_lcds(&std::collections::HashSet::from(["lcd:01".into()]));
        assert_eq!(health.findings().len(), 2);
        health.retain_lcds(&std::collections::HashSet::new());
        assert_eq!(health.findings().len(), 1);
        assert_eq!(health.findings()[0].code, "device.open.hid:shared");
    }

    #[test]
    fn wireless_recovery_clears_only_its_own_bounded_failure() {
        let mut health = StateHealth::default();
        health.device_open_failed("hid:controller", "busy");
        health.wireless_unavailable(&"é".repeat(3000));
        assert_eq!(health.findings()[0].evidence.chars().count(), 2048);
        health.wireless_unavailable("No transmitter detected");
        let findings = health.findings();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].code, "device.wireless.initialization");
        assert_eq!(findings[0].evidence, "No transmitter detected");
        assert_eq!(findings[0].state, CheckState::Failed);
        health.wireless_ready();
        assert_eq!(health.findings().len(), 1);
        assert_eq!(health.findings()[0].code, "device.open.hid:controller");
    }

    #[test]
    fn device_failures_replace_prior_errors_and_clear_on_recovery_or_removal() {
        let mut health = StateHealth::default();
        health.config_failed("invalid JSON", true);
        health.device_open_failed("hid:a", "permission denied");
        health.device_open_failed("hid:a", "device busy");
        health.device_open_failed("hid:b", "timeout");
        let findings = health.findings();
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[1].code, "device.open.hid:a");
        assert_eq!(findings[1].evidence, "device busy");
        assert_eq!(findings[1].state, CheckState::Failed);
        health.device_opened("hid:a");
        assert_eq!(health.findings().len(), 2);
        health.retain_devices(&std::collections::HashSet::new());
        assert_eq!(health.findings().len(), 1);
        assert_eq!(health.findings()[0].code, "configuration.load");
    }

    #[test]
    fn device_failure_reports_bound_storage_and_expose_overflow() {
        let mut health = StateHealth::default();
        for id in 0..100 {
            health.device_open_failed(&format!("hid:{id:03}"), &"é".repeat(3000));
        }
        let findings = health.findings();
        assert_eq!(findings.len(), 65);
        assert_eq!(findings[0].evidence.chars().count(), 2048);
        assert_eq!(findings[64].code, "device.open.more");
        health.device_open_failed("hid:000", "changed failure");
        assert_eq!(health.findings()[0].evidence, "changed failure");
        health.retain_devices(&std::collections::HashSet::from(["hid:000".into()]));
        assert_eq!(health.findings().len(), 1);
        health.device_opened("hid:000");
        assert!(health.findings().is_empty());
    }

    #[test]
    fn failed_reload_reports_retained_state_and_success_clears_the_failure() {
        let mut health = StateHealth::default();
        health.config_failed("invalid JSON", false);
        assert!(health.findings()[0].evidence.contains("No configuration"));
        health.config_failed("unreadable file", true);
        assert!(health.findings()[0].evidence.contains("remains active"));
        health.config_loaded(&[], true);
        assert_eq!(health.findings()[0].state, CheckState::Passed);
        health.config_loaded(&[], false);
        assert_eq!(health.findings()[0].state, CheckState::Unavailable);
        assert!(health.findings()[0].evidence.contains("deferred"));
    }

    #[test]
    fn warning_evidence_is_bounded_and_template_recovery_is_independent() {
        let mut health = StateHealth::default();
        health.config_loaded(&vec!["é".repeat(500); 20], true);
        health.templates_failed("template JSON is invalid");
        let findings = health.findings();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert!(findings[0].evidence.contains("20 load/validation"));
        assert!(findings[0].evidence.chars().count() < 3300);
        assert_eq!(findings[1].severity, FindingSeverity::Error);
        health.templates_loaded(&[]);
        assert_eq!(health.findings()[0].state, CheckState::Failed);
        assert_eq!(health.findings()[1].state, CheckState::Passed);
    }
}
