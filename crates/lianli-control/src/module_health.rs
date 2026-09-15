use anyhow::{ensure, Context, Result};
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationContext, InstallationFinding, InstallationGuide,
};
use std::time::{Duration, Instant};

enum ModuleFile {
    Installed(String),
    Missing,
    Unverified(String),
}

fn module_file(output: Result<crate::command::Output>, module: &str) -> ModuleFile {
    match output {
        Ok(output) if output.status.success() && !output.stdout.trim().is_empty() => {
            ModuleFile::Installed(output.stdout.trim().into())
        }
        Ok(output)
            if output
                .stderr
                .contains(&format!("Module {module} not found")) =>
        {
            ModuleFile::Missing
        }
        Ok(output) => ModuleFile::Unverified(format!("Host modinfo: {}", output.stderr.trim())),
        Err(error) => ModuleFile::Unverified(format!("Host modinfo: {error:#}")),
    }
}

pub fn collect(context: &InstallationContext) -> Vec<InstallationFinding> {
    let context = context.clone();
    crate::runtime_health::run_check(move || inspect(&context), Duration::from_secs(12))
        .unwrap_or_else(|error| vec![unavailable("modules", &format!("{error:#}"))])
}

fn unavailable(module: &str, error: &str) -> InstallationFinding {
    InstallationFinding {
        code: format!("host.module.{module}"),
        state: CheckState::Unavailable,
        severity: FindingSeverity::Info,
        feature: "Optional desktop backend".into(),
        context: "Host kernel".into(),
        title: format!("Optional display module: {module}"),
        evidence: error.into(),
        remediation: "Check the host installation using the desktop backend guide. Hyprland needs no optional display module.".into(),
        guide: InstallationGuide::Troubleshooting,
    }
}

fn inspect(context: &InstallationContext) -> Result<Vec<InstallationFinding>> {
    let route = crate::services::Route::detect(context)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let query = |program: &str, args: &[&str]| -> Result<String> {
        ensure!(
            Instant::now() < deadline,
            "Module checks reached their time limit"
        );
        let output = route.diagnostic_output(program, args)?;
        ensure!(
            output.status.success(),
            "{}: {}",
            program,
            output.stderr.trim()
        );
        Ok(output.stdout)
    };
    let kernel = query("/usr/bin/uname", &["-r"])?;
    let kernel = kernel.trim();
    ensure!(
        !kernel.is_empty()
            && kernel.len() <= 128
            && kernel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte)),
        "Host returned an invalid kernel release"
    );
    let dkms = query("/usr/sbin/dkms", &["status", "-k", kernel]);
    let journal = query(
        "/usr/bin/journalctl",
        &[
            "--kernel",
            "--boot=0",
            "--quiet",
            "--no-pager",
            "--lines=80",
            "--grep=(?i)(evdi|hermes[-_]kms)",
            "--output=json",
            "--output-fields=MESSAGE",
        ],
    )
    .and_then(|text| journal_messages(&text));
    let mut findings = Vec::new();
    for module in ["hermes_kms", "evdi"] {
        let result = (|| -> Result<InstallationFinding> {
            ensure!(
                Instant::now() < deadline,
                "Module checks reached their time limit"
            );
            let loaded = route
                .diagnostic_output("/usr/bin/test", &["-d", &format!("/sys/module/{module}")])?;
            ensure!(
                matches!(loaded.status.code(), Some(0 | 1)),
                "Cannot inspect loaded host modules"
            );
            if loaded.status.success() {
                return Ok(classify(
                    module,
                    kernel,
                    true,
                    &ModuleFile::Missing,
                    false,
                    None,
                    journal.as_deref(),
                ));
            }
            ensure!(
                Instant::now() < deadline,
                "Module checks reached their time limit"
            );
            let installed = module_file(
                route.diagnostic_output(
                    "/usr/sbin/modinfo",
                    &["-k", kernel, "-F", "filename", module],
                ),
                module,
            );
            let registered = dkms
                .as_ref()
                .is_ok_and(|text| dkms_registered(text, module));
            let headers = if registered && matches!(installed, ModuleFile::Missing) {
                ensure!(
                    Instant::now() < deadline,
                    "Module checks reached their time limit"
                );
                let output = route.diagnostic_output(
                    "/usr/bin/test",
                    &["-f", &format!("/lib/modules/{kernel}/build/Makefile")],
                )?;
                ensure!(
                    matches!(output.status.code(), Some(0 | 1)),
                    "Cannot inspect host kernel build files"
                );
                Some(output.status.success())
            } else {
                None
            };
            let mut finding = classify(
                module,
                kernel,
                false,
                &installed,
                registered,
                headers,
                journal.as_deref(),
            );
            if dkms.is_err() {
                finding.evidence.push_str(" Host DKMS status is unavailable. Builds managed by another installer are not inspected.");
            }
            Ok(finding)
        })();
        findings.push(result.unwrap_or_else(|error| unavailable(module, &format!("{error:#}"))));
    }
    Ok(findings)
}

fn dkms_registered(text: &str, module: &str) -> bool {
    text.lines().any(|line| {
        line.split_once('/')
            .is_some_and(|(name, _)| name.replace('-', "_") == module)
    })
}

fn journal_messages(text: &str) -> Result<Vec<String>> {
    text.lines()
        .take(81)
        .map(|line| {
            let record: serde_json::Value = serde_json::from_str(line)?;
            Ok(record
                .get("MESSAGE")
                .and_then(serde_json::Value::as_str)
                .context("Kernel journal message is not readable text")?
                .to_owned())
        })
        .collect()
}

fn failure_message<'a>(messages: &'a [String], module: &str) -> Option<&'a str> {
    messages.iter().rev().find_map(|message| {
        let text = message.to_ascii_lowercase().replace('-', "_");
        let names_module = text
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| word == module);
        (names_module
            && [
                "key was rejected",
                "required key not available",
                "loading of unsigned module is rejected",
                "version magic",
                "unknown symbol",
                "failed to load",
            ]
            .iter()
            .any(|error| text.contains(error)))
        .then_some(message.as_str())
    })
}

fn classify(
    module: &str,
    kernel: &str,
    loaded: bool,
    installed: &ModuleFile,
    registered: bool,
    headers: Option<bool>,
    journal: Result<&[String], &anyhow::Error>,
) -> InstallationFinding {
    let mut finding = unavailable(module, "");
    if loaded {
        finding.state = CheckState::Passed;
        finding.evidence = format!("Loaded in host kernel {kernel}. Output ownership, mode, capture and encoding are checked separately during desktop startup.");
        finding.remediation.clear();
    } else if let Some(error) = journal
        .as_ref()
        .ok()
        .and_then(|messages| failure_message(messages, module))
    {
        finding.state = CheckState::Failed;
        finding.severity = FindingSeverity::Warning;
        finding.evidence =
            format!("Not loaded. Host kernel reported this during the current boot: {error}");
        finding.remediation = "Review the host kernel log, then repair module compatibility or signing. Retry desktop mode after repair.".into();
    } else if registered && matches!(installed, ModuleFile::Missing) {
        finding.state = CheckState::Failed;
        finding.severity = FindingSeverity::Warning;
        finding.evidence = format!("DKMS lists {module}, but modinfo reports no installed module for host kernel {kernel}.");
        finding.remediation = if headers == Some(false) {
            "Matching host kernel build files are missing. Install the kernel development package, then repair the DKMS build using the host package instructions."
        } else {
            "Review the host DKMS build log and finish installing the module for the running kernel."
        }.into();
    } else if let ModuleFile::Installed(path) = installed {
        finding.state = CheckState::Passed;
        finding.evidence = format!("Installed for host kernel {kernel}: {}. Not loaded. This does not verify signing trust or desktop operation.", path.trim());
        finding.remediation =
            "If this backend is needed, follow the host driver startup instructions.".into();
    } else if let ModuleFile::Unverified(error) = installed {
        finding.evidence = error.clone();
    } else {
        finding.state = CheckState::NotApplicable;
        finding.evidence = format!("Optional module is not installed for host kernel {kernel}.");
        finding.remediation = "Install this host driver only if you need its desktop backend. Hyprland uses native headless output.".into();
    }
    if !loaded && journal.is_err() {
        finding
            .evidence
            .push_str(" Kernel load and signature errors could not be checked from this account.");
    }
    finding
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_module_without_headers_guides_host_repair() {
        let finding = classify(
            "evdi",
            "6.1",
            false,
            &ModuleFile::Missing,
            true,
            Some(false),
            Ok(&[]),
        );
        assert_eq!(finding.state, CheckState::Failed);
        assert!(finding.remediation.contains("build files are missing"));
        assert!(dkms_registered("evdi/1.15.0, 6.1, x86_64: added", "evdi"));
        assert!(!dkms_registered("other-evdi/1, 6.1: installed", "evdi"));
        assert!(dkms_registered(
            "hermes-kms/1, 6.1: installed",
            "hermes_kms"
        ));
    }

    #[test]
    fn loaded_module_supersedes_old_failure_without_claiming_capture_works() {
        let messages = vec!["evdi: Key was rejected by service".into()];
        let failed = classify(
            "evdi",
            "6.1",
            false,
            &ModuleFile::Missing,
            false,
            None,
            Ok(&messages),
        );
        assert_eq!(failed.state, CheckState::Failed);
        let loaded = classify(
            "evdi",
            "6.1",
            true,
            &ModuleFile::Missing,
            false,
            None,
            Ok(&messages),
        );
        assert_eq!(loaded.state, CheckState::Passed);
        assert!(loaded.evidence.contains("checked separately"));
    }

    #[test]
    fn missing_optional_module_does_not_trigger_a_warning() {
        let finding = classify(
            "evdi",
            "6.1",
            false,
            &ModuleFile::Missing,
            false,
            None,
            Ok(&[]),
        );
        assert_eq!(finding.state, CheckState::NotApplicable);
        assert_eq!(finding.severity, FindingSeverity::Info);
    }

    #[test]
    fn journal_ignores_unrelated_modules_and_permitted_unsigned_loads() {
        let messages = journal_messages("{\"MESSAGE\":\"nvidia: Key was rejected by service\"}\n{\"MESSAGE\":\"evdi: module verification failed: signature and/or required key missing - tainting kernel\"}").unwrap();
        assert!(failure_message(&messages, "evdi").is_none());
        assert!(failure_message(&["other_evdi: failed to load".into()], "evdi").is_none());
        assert!(failure_message(
            &["hermes-kms: Unknown symbol something".into()],
            "hermes_kms"
        )
        .is_some());
        assert!(journal_messages("broken").is_err());
    }

    #[test]
    fn unavailable_tools_do_not_claim_a_missing_module_or_failed_build() {
        let file = module_file(Err(anyhow::anyhow!("permission denied")), "evdi");
        let finding = classify("evdi", "6.1", false, &file, true, None, Ok(&[]));
        assert_eq!(finding.state, CheckState::Unavailable);
        assert_eq!(finding.severity, FindingSeverity::Info);
        assert!(finding.evidence.contains("permission denied"));
    }

    #[test]
    fn modinfo_distinguishes_missing_installed_and_unreadable() {
        use std::os::unix::process::ExitStatusExt;
        let output = |status, stdout: &str, stderr: &str| {
            Ok(crate::command::Output {
                status: std::process::ExitStatus::from_raw(status),
                stdout: stdout.into(),
                stderr: stderr.into(),
            })
        };
        assert!(matches!(
            module_file(
                output(256, "", "modinfo: ERROR: Module evdi not found."),
                "evdi"
            ),
            ModuleFile::Missing
        ));
        assert!(matches!(
            module_file(
                output(256, "", "modinfo: ERROR: Module other not found."),
                "evdi"
            ),
            ModuleFile::Unverified(_)
        ));
        let file = module_file(
            output(0, "/lib/modules/6.1/extra/evdi.ko.zst\n", ""),
            "evdi",
        );
        let finding = classify("evdi", "6.1", false, &file, false, None, Ok(&[]));
        assert_eq!(finding.state, CheckState::Passed);
        assert!(finding.evidence.contains("does not verify signing trust"));
        assert!(matches!(
            module_file(output(0, "", ""), "evdi"),
            ModuleFile::Unverified(_)
        ));
    }
}
