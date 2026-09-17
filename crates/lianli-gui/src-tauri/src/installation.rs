use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationContext, InstallationFinding, InstallationGuide,
    InstallationReport,
};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PACKAGED_RULES: &str = include_str!("../../../../packaging/udev/60-lianli.rules");
const RULE_PATHS: &[&str] = &[
    "etc/udev/rules.d/60-lianli.rules",
    "run/udev/rules.d/60-lianli.rules",
    "usr/local/lib/udev/rules.d/60-lianli.rules",
    "usr/lib/udev/rules.d/60-lianli.rules",
    "lib/udev/rules.d/60-lianli.rules",
];
const MAX_RULE_BYTES: u64 = 128 * 1024;
#[derive(Default)]
struct Cache {
    report: Option<(Instant, InstallationReport)>,
    running: bool,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    report: None,
    running: false,
});

struct CheckGuard;

impl Drop for CheckGuard {
    fn drop(&mut self) {
        CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .running = false;
    }
}

pub fn check() -> Result<InstallationReport, String> {
    {
        let mut cache = CACHE
            .lock()
            .map_err(|_| "Installation cache is unavailable".to_string())?;
        if let Some((time, report)) = &cache.report {
            if time.elapsed() < Duration::from_secs(2) {
                return Ok(report.clone());
            }
        }
        if cache.running {
            return Err("An installation check is already running. Try again shortly.".into());
        }
        cache.running = true;
    }
    let _guard = CheckGuard;
    let context = InstallationContext::detect();
    let (root, label) = match &context {
        InstallationContext::Native => (Some(Path::new("/")), "Host"),
        InstallationContext::Distrobox { .. } => {
            (Some(Path::new("/run/host")), "Host via Distrobox")
        }
        InstallationContext::UnsupportedContainer => (None, "Container. Host unavailable"),
    };
    let mut findings = Vec::new();
    let mut daemon_context = None;
    findings.push(check_desktop_startup(&context));
    if let Some(root) = root {
        findings.push(check_rules(root, label));
        if let Some(lock) = context.daemon_lock_path() {
            findings.push(check_lock(&lock, label));
        }
    } else {
        findings.push(InstallationFinding {
            code: "host.unavailable".into(),
            state: CheckState::Unavailable,
            severity: FindingSeverity::Error,
            feature: "Installation".into(),
            context: label.into(),
            title: "Host integration is unavailable".into(),
            evidence: "This container does not expose the supported Distrobox host integration.".into(),
            remediation: "Use a native installation or restore Distrobox's /run/host integration. The daemon cannot safely start with a private container lock.".into(),
            guide: InstallationGuide::Distrobox,
        });
    }
    let services = lianli_control::services::inspect(&context);
    findings.extend(lianli_control::services::findings(&services));
    findings.extend(lianli_control::lingering::finding(&services));
    findings.extend(lianli_control::module_health::collect(&context));
    let desktop = std::env::current_exe()
        .map_err(|error| error.to_string())
        .and_then(|path| {
            lianli_control::runtime_health::collect(&path.with_file_name("lianli-control"))
                .map_err(|error| format!("{error:#}"))
        });
    match desktop {
        Ok(mut checks) => {
            for check in &mut checks {
                check.code = format!("desktop.{}", check.code);
                check.context = format!("Desktop user: {}", check.context);
            }
            findings.extend(checks);
        }
        Err(error) => findings.push(runtime_unavailable(
            "desktop.runtime",
            "Desktop user",
            &error,
        )),
    }
    let info = crate::ipc::request("GetDaemonInfo", serde_json::Value::Null).and_then(|data| {
        serde_json::from_value::<lianli_shared::daemon::DaemonInfo>(data)
            .map_err(|error| error.to_string())
    });
    match info {
        Ok(info) => {
            if let Some(lianli_shared::services::ServiceProbe::Known { value }) =
                &services.ownership
            {
                if let Some(finding) =
                    compare_daemon_lock(info.ownership_lock.as_ref(), &value.identity, label)
                {
                    findings.push(finding);
                }
            }
            let runtime = if info
                .capabilities
                .iter()
                .any(|value| value == lianli_shared::daemon::INSTALLATION_HEALTH)
            {
                crate::ipc::request("GetInstallationHealth", serde_json::Value::Null)
                    .and_then(|data| {
                        serde_json::from_value::<
                            lianli_shared::installation::RuntimeInstallationReport,
                        >(data)
                        .map_err(|error| error.to_string())
                    })
                    .and_then(|report| {
                        let context = report.context.clone();
                        let findings = merge_runtime(&info.instance_id, report)?;
                        daemon_context = context;
                        Ok(findings)
                    })
            } else {
                Err("The connected daemon lacks runtime installation checks. Rebuild/install matching daemon and control binaries, then restart the selected service.".into())
            };
            match runtime {
                Ok(checks) => {
                    reconcile_rules(&mut findings, &checks);
                    findings.extend(checks);
                }
                Err(error) => findings.push(runtime_unavailable(
                    "runtime.unavailable",
                    "Selected daemon",
                    &error,
                )),
            }
        }
        Err(error) => {
            let owner_pid = match &services.ownership {
                Some(lianli_shared::services::ServiceProbe::Known { value }) => value.owner_pid,
                _ => None,
            };
            findings.push(daemon_unavailable(owner_pid, &error));
        }
    }
    let report = InstallationReport {
        services: Some(services),
        context,
        daemon_context,
        checked_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        findings,
    };
    CACHE
        .lock()
        .map_err(|_| "Installation cache is unavailable".to_string())?
        .report = Some((Instant::now(), report.clone()));
    Ok(report)
}

fn check_desktop_startup(context: &InstallationContext) -> InstallationFinding {
    if let InstallationContext::Distrobox { name } = context {
        return lianli_control::services::check_distrobox_desktop_startup(name);
    }
    let native = matches!(context, InstallationContext::Native);
    let root = Path::new("/usr/lib/systemd/user");
    let unit = fs::File::open(root.join("lianli-session.service")).and_then(|file| {
        let mut contents = String::new();
        file.take(4097).read_to_string(&mut contents)?;
        Ok(contents)
    });
    let present = native
        && unit.is_ok_and(|contents| contents.len() <= 4096 && contents.contains("--login-start"))
        && fs::read_link(root.join("default.target.wants/lianli-session.service"))
            .is_ok_and(|target| target == Path::new("../lianli-session.service"));
    InstallationFinding {
        code: "desktop.login_startup".into(),
        state: if present { CheckState::Passed } else { CheckState::Unavailable },
        severity: if present { FindingSeverity::Info } else { FindingSeverity::Warning },
        feature: "Desktop display login startup".into(),
        context: "Desktop user".into(),
        title: if present { "Packaged desktop login startup files found" } else { "Automatic desktop login startup is not established" }.into(),
        evidence: if present {
            "The login-discovery unit and default-target link are installed. This file check does not verify user overrides, a masked service, or successful capture."
        } else {
            "The native login-discovery unit and default-target link were not found. Launching the GUI can start capture for this login but does not establish next-login startup."
        }.into(),
        remediation: "Check lianli-session.service or follow the desktop startup guide for source builds. This helper is needed only for Desktop mode.".into(),
        guide: InstallationGuide::Troubleshooting,
    }
}

fn reconcile_rules(findings: &mut [InstallationFinding], daemon: &[InstallationFinding]) {
    if !daemon
        .iter()
        .any(|finding| finding.code == "usb.node_access" && finding.state == CheckState::Passed)
    {
        return;
    }
    if let Some(rules) = findings
        .iter_mut()
        .find(|finding| finding.code == "usb.rules" && finding.state == CheckState::Unavailable)
    {
        rules.severity = FindingSeverity::Info;
        rules.evidence.push_str(" The daemon has node access for visible devices. Custom rules or ACLs may provide it. Device initialization and devices outside this environment remain unverified.");
        rules.remediation = "No rule replacement is required. Keep working custom rules. Check daemon logs for open failures and Recheck after setup changes.".into();
    }
}

fn merge_runtime(
    instance: &str,
    mut report: lianli_shared::installation::RuntimeInstallationReport,
) -> Result<Vec<InstallationFinding>, String> {
    if report.instance_id != instance || report.findings.len() > 128 {
        return Err("The daemon changed during runtime checks or returned an invalid report. Recheck the selected service.".into());
    }
    for finding in &mut report.findings {
        finding.context = format!("Daemon UID {}: {}", report.uid, finding.context);
    }
    Ok(report.findings)
}

fn daemon_unavailable(owner_pid: Option<u32>, error: &str) -> InstallationFinding {
    let detail: String = error.chars().take(2048).collect();
    let (title, evidence, remediation) = match owner_pid {
        Some(pid) => (
            "Hardware owner unreachable",
            format!("PID {pid} holds the hardware lock, but the GUI cannot reach daemon IPC. {detail}"),
            "Wait for startup or switching to finish, then Recheck. If this persists, check daemon logs and socket visibility. For a Distrobox daemon, launch the GUI in the same box or expose the host runtime socket.",
        ),
        None => (
            "Daemon connection unavailable",
            format!("The GUI cannot reach the daemon. {detail}"),
            "Choose one service in Settings or start your manual daemon, then Recheck. For Distrobox, the GUI and daemon must share a visible runtime socket.",
        ),
    };
    InstallationFinding {
        code: "daemon.connection".into(),
        state: CheckState::Unavailable,
        severity: FindingSeverity::Warning,
        feature: "Daemon connection".into(),
        context: "GUI connection".into(),
        title: title.into(),
        evidence,
        remediation: remediation.into(),
        guide: InstallationGuide::Troubleshooting,
    }
}

fn runtime_unavailable(code: &str, context: &str, evidence: &str) -> InstallationFinding {
    InstallationFinding { code:code.into(), state:CheckState::Unavailable, severity:FindingSeverity::Warning,
        feature:"Installation checks".into(), context:context.into(), title:"Runtime installation checks unavailable".into(),
        evidence:evidence.into(), remediation:"Install the matching lianli-control beside the GUI and daemon, start the selected service, then Recheck. See the troubleshooting guide for source builds and offline diagnostics.".into(), guide:InstallationGuide::Troubleshooting }
}

fn compare_daemon_lock(
    held: Option<&lianli_shared::daemon::FileIdentity>,
    host: &lianli_shared::daemon::FileIdentity,
    context: &str,
) -> Option<InstallationFinding> {
    if held == Some(host) {
        return None;
    }
    let (state, severity, evidence) = if held.is_some() {
        (CheckState::Failed, FindingSeverity::Error,
            "The connected daemon holds a different file from the verified host ownership lock. Separate launch routes may not exclude each other.")
    } else {
        (CheckState::Unavailable, FindingSeverity::Warning,
            "The connected daemon does not report its held lock identity, so cross-mode ownership cannot be verified.")
    };
    Some(InstallationFinding {
        code: "ownership.daemon".into(), state, severity, feature: "Daemon ownership".into(), context: context.into(),
        title: "Connected daemon lock identity".into(), evidence: evidence.into(),
        remediation: "Update all daemon launch routes, stop the old owner cleanly and restart the selected service. Do not delete a live lock file or start another daemon to repair this.".into(),
        guide: InstallationGuide::ServiceModes,
    })
}

fn check_lock(path: &Path, context: &str) -> InstallationFinding {
    let result = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .and_then(|file| {
            if file.metadata()?.is_file() {
                Ok(())
            } else {
                Err(io::Error::other("The ownership lock is not a regular file"))
            }
        });
    let (state, severity, evidence) = match result {
        Ok(()) => (
            CheckState::Passed,
            FindingSeverity::Info,
            format!("{} is a regular file accessible to this GUI user. This check does not acquire the lock or verify another account's access.", path.display()),
        ),
        Err(error) => (
            CheckState::Failed,
            FindingSeverity::Error,
            format!("{}: {error}", path.display()),
        ),
    };
    InstallationFinding {
        code: "ownership.lock".into(),
        state,
        severity,
        feature: "Daemon startup".into(),
        context: context.into(),
        title: "Shared daemon ownership lock".into(),
        evidence,
        remediation: "Install the supplied tmpfiles rule on the host and run sudo systemd-tmpfiles --create lianli.conf. Never delete or replace a lock while a daemon is running.".into(),
        guide: InstallationGuide::ServiceModes,
    }
}

fn check_rules(root: &Path, context: &str) -> InstallationFinding {
    let mut finding = InstallationFinding {
        code: "usb.rules".into(),
        state: CheckState::Unavailable,
        severity: FindingSeverity::Warning,
        feature: "USB access".into(),
        context: context.into(),
        title: "Packaged USB permission rules".into(),
        evidence: "60-lianli.rules was not found in the host's udev rule directories. Other custom rules may still provide access.".into(),
        remediation: "Install the current USB rules on the host, reload udev rules and reapply them. Review intentional custom rules before replacing them. File checks alone do not verify device permissions.".into(),
        guide: InstallationGuide::UsbPermissions,
    };
    for name in RULE_PATHS {
        let resolved = match resolve_in_root(root, Path::new(name)) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                finding.evidence = format!("Cannot inspect /{name}: {error}");
                return finding;
            }
        };
        if resolved == root.join("dev/null") {
            finding.evidence = format!("/{name} disables the packaged rules by pointing to /dev/null. Other custom rules may still provide access.");
            return finding;
        }
        match read_rules(&resolved) {
            Ok(content) if active_rules(&content) == active_rules(PACKAGED_RULES) => {
                finding.state = CheckState::Passed;
                finding.severity = FindingSeverity::Info;
                finding.evidence = format!("/{name} matches this application's packaged rules. Other rule filenames, reload status and actual daemon device access have not been verified.");
            }
            Ok(content) => {
                let description = if active_rules(&content).is_empty() {
                    "contains no active rules"
                } else {
                    "differs from this application's packaged rules"
                };
                finding.evidence = format!("/{name} {description}. It overrides lower-priority copies with the same name. It may be outdated or customized. Device access remains unverified.");
            }
            Err(error) => finding.evidence = format!("Cannot read /{name}: {error}"),
        }
        return finding;
    }
    finding
}

fn active_rules(content: &str) -> Vec<&str> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn read_rules(path: &Path) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Not a regular rule file"));
    }
    let mut content = String::new();
    file.take(MAX_RULE_BYTES + 1).read_to_string(&mut content)?;
    if content.len() as u64 > MAX_RULE_BYTES {
        return Err(io::Error::other(
            "Rule file exceeds the 128 KiB check limit",
        ));
    }
    Ok(content)
}

fn resolve_in_root(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let mut pending: VecDeque<_> = path
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut relative = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => relative.clear(),
            Some(Component::ParentDir) => {
                relative.pop();
            }
            Some(Component::Normal(name)) => {
                relative.push(name);
                let candidate = root.join(&relative);
                if candidate == root.join("dev/null") {
                    continue;
                }
                let metadata = fs::symlink_metadata(&candidate).map_err(|error| {
                    if links > 0 && error.kind() == io::ErrorKind::NotFound {
                        io::Error::other("Rule path contains a dangling symlink")
                    } else {
                        error
                    }
                })?;
                if metadata.is_symlink() {
                    links += 1;
                    if links > 40 {
                        return Err(io::Error::other("Too many rule path symlinks"));
                    }
                    let target = fs::read_link(candidate)?;
                    relative.pop();
                    for component in target.components().rev() {
                        pending.push_front(component.as_os_str().to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_owner_guidance_does_not_misdiagnose_a_missing_helper() {
        let held = daemon_unavailable(Some(1234), &"界".repeat(3000));
        assert_eq!(held.code, "daemon.connection");
        assert_eq!(held.title, "Hardware owner unreachable");
        assert!(held.evidence.contains("PID 1234 holds the hardware lock"));
        assert_eq!(held.evidence.matches('界').count(), 2048);
        assert!(held.remediation.contains("socket visibility"));
        assert!(!held.remediation.contains("Install the matching"));
        let unknown = daemon_unavailable(None, "Connection refused");
        assert_eq!(unknown.title, "Daemon connection unavailable");
        assert!(!unknown.evidence.contains("holds the hardware lock"));
        assert!(unknown.evidence.contains("Connection refused"));
    }

    #[test]
    fn custom_rules_require_daemon_node_permission_evidence() {
        let root = tempfile::tempdir().unwrap();
        let mut node = runtime_unavailable("usb.node_access", "Daemon", "fixture");
        for state in [
            CheckState::Failed,
            CheckState::Unavailable,
            CheckState::NotApplicable,
        ] {
            node.state = state;
            let mut rules = vec![check_rules(root.path(), "Host")];
            reconcile_rules(&mut rules, &[node.clone()]);
            assert_eq!(rules[0].severity, FindingSeverity::Warning);
        }
        node.state = CheckState::Passed;
        let mut rules = vec![check_rules(root.path(), "Host")];
        reconcile_rules(&mut rules, &[node.clone()]);
        assert_eq!(rules[0].severity, FindingSeverity::Info);
        assert_eq!(rules[0].state, CheckState::Unavailable);
        assert!(rules[0].remediation.contains("No rule replacement"));
        node.code = "desktop.usb.node_access".into();
        let mut rules = vec![check_rules(root.path(), "Host")];
        reconcile_rules(&mut rules, &[node]);
        assert_eq!(rules[0].severity, FindingSeverity::Warning);
    }

    #[test]
    fn runtime_results_belong_to_the_observed_daemon_instance() {
        let report = || lianli_shared::installation::RuntimeInstallationReport {
            instance_id: "original".into(),
            uid: 2000,
            context: None,
            findings: vec![runtime_unavailable("runtime.test", "Native", "fixture")],
        };
        assert!(merge_runtime("replacement", report()).is_err());
        let checks = merge_runtime("original", report()).unwrap();
        assert_eq!(checks[0].context, "Daemon UID 2000: Native");
        let mut device_errors = report();
        device_errors
            .findings
            .resize(128, device_errors.findings[0].clone());
        let checks = merge_runtime("original", device_errors).unwrap();
        assert_eq!(checks.len(), 128);
        assert!(checks
            .iter()
            .all(|finding| finding.context == "Daemon UID 2000: Native"));
        let mut oversized = report();
        oversized
            .findings
            .resize(129, oversized.findings[0].clone());
        assert!(merge_runtime("original", oversized).is_err());
    }

    #[test]
    fn daemon_lock_mismatch_is_an_error_while_legacy_identity_is_unverified() {
        let host = lianli_shared::daemon::FileIdentity {
            device: "47".into(),
            inode: "9007199254740993".into(),
        };
        assert!(compare_daemon_lock(Some(&host), &host, "Host").is_none());
        let legacy = compare_daemon_lock(None, &host, "Host").unwrap();
        assert_eq!(legacy.state, CheckState::Unavailable);
        let other = lianli_shared::daemon::FileIdentity {
            inode: "9007199254740992".into(),
            ..host.clone()
        };
        let finding = compare_daemon_lock(Some(&other), &host, "Host").unwrap();
        assert_eq!(finding.state, CheckState::Failed);
        assert_eq!(finding.severity, FindingSeverity::Error);
    }
    use std::os::unix::fs::symlink;

    fn install(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn current_vendor_rules_do_not_hide_an_outdated_administrator_override() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), RULE_PATHS[3], PACKAGED_RULES);
        assert_eq!(check_rules(root.path(), "Host").state, CheckState::Passed);
        install(
            root.path(),
            RULE_PATHS[0],
            "SUBSYSTEM==\"usb\", MODE=\"0600\"",
        );
        let report = check_rules(root.path(), "Host");
        assert_eq!(report.state, CheckState::Unavailable);
        assert!(report
            .evidence
            .contains("/etc/udev/rules.d/60-lianli.rules"));
        assert!(report.evidence.contains("customized"));
    }

    #[test]
    fn masking_and_empty_overrides_are_not_reported_as_current_rules() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), RULE_PATHS[3], PACKAGED_RULES);
        install(root.path(), RULE_PATHS[0], "# disabled\n");
        assert!(check_rules(root.path(), "Host")
            .evidence
            .contains("no active rules"));
        fs::remove_file(root.path().join(RULE_PATHS[0])).unwrap();
        fs::create_dir_all(root.path().join("dev")).unwrap();
        symlink("/dev/null", root.path().join(RULE_PATHS[0])).unwrap();
        assert!(check_rules(root.path(), "Host")
            .evidence
            .contains("pointing to /dev/null"));
    }

    #[test]
    fn absolute_host_symlinks_do_not_resolve_inside_the_container() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), RULE_PATHS[3], PACKAGED_RULES);
        fs::create_dir_all(root.path().join("etc/udev/rules.d")).unwrap();
        symlink(
            format!("/{}", RULE_PATHS[3]),
            root.path().join(RULE_PATHS[0]),
        )
        .unwrap();
        assert_eq!(
            check_rules(root.path(), "Host via Distrobox").state,
            CheckState::Passed
        );
    }

    #[test]
    fn unreadable_override_targets_do_not_fall_back_to_healthy_vendor_rules() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), RULE_PATHS[3], PACKAGED_RULES);
        fs::create_dir_all(root.path().join("etc/udev/rules.d")).unwrap();
        let path = root.path().join(RULE_PATHS[0]);
        symlink("/missing", &path).unwrap();
        assert_eq!(
            check_rules(root.path(), "Host").state,
            CheckState::Unavailable
        );
        fs::remove_file(&path).unwrap();
        symlink("60-lianli.rules", &path).unwrap();
        assert!(check_rules(root.path(), "Host")
            .evidence
            .contains("Too many"));
    }

    #[test]
    fn missing_rules_and_comment_only_differences_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            check_rules(root.path(), "Host").state,
            CheckState::Unavailable
        );
        install(
            root.path(),
            RULE_PATHS[3],
            &format!("# local note\n\n{PACKAGED_RULES}"),
        );
        assert_eq!(check_rules(root.path(), "Host").state, CheckState::Passed);
        install(
            root.path(),
            RULE_PATHS[3],
            &"x".repeat(MAX_RULE_BYTES as usize + 1),
        );
        assert!(check_rules(root.path(), "Host")
            .evidence
            .contains("128 KiB"));
    }

    #[test]
    fn lock_check_never_creates_or_modifies_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        assert_eq!(check_lock(&path, "Host").state, CheckState::Failed);
        assert!(!path.exists());
        fs::write(&path, "12345").unwrap();
        assert_eq!(check_lock(&path, "Host").state, CheckState::Passed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "12345");
        let alias = root.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert_eq!(check_lock(&alias, "Host").state, CheckState::Failed);
    }
}
