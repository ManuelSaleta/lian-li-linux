use anyhow::{ensure, Context, Result};
use lianli_shared::config::HidBackend;
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationContext, InstallationFinding, InstallationGuide,
};
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(false);

struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub version: String,
    pub uid: u32,
    pub gid: u32,
    pub groups_fingerprint: String,
    #[serde(default)]
    pub hid_backend: Option<HidBackend>,
    pub findings: Vec<InstallationFinding>,
}

fn access(path: &Path) -> Result<(u32, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "The shared ownership lock is not a regular file"
    );
    Ok((metadata.gid(), effective_access(&file)?))
}

pub(crate) fn effective_access(file: &std::fs::File) -> Result<bool> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::R_OK | libc::W_OK,
            libc::AT_EACCESS | libc::AT_EMPTY_PATH,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::EACCES | libc::EPERM | libc::EROFS)
    ) {
        return Ok(false);
    }
    Err(error.into())
}

fn findings(
    context: &InstallationContext,
    uid: u32,
    groups: &[u32],
    configured: Result<Vec<u32>>,
    access: Result<(u32, bool)>,
) -> Vec<InstallationFinding> {
    let label = match context {
        InstallationContext::Native => format!("Native process UID {uid}"),
        InstallationContext::Distrobox { name } => format!("Distrobox {name}, process UID {uid}"),
        InstallationContext::UnsupportedContainer => format!("Unsupported container, UID {uid}"),
    };
    let guide = if matches!(context, InstallationContext::Native) {
        InstallationGuide::ServiceModes
    } else {
        InstallationGuide::Distrobox
    };
    let repair = if matches!(context, InstallationContext::Distrobox { .. }) {
        "Apply the host tmpfiles/group setup, log out and back in, then stop and re-enter the existing box and restart its user service. Compare numeric group IDs visible inside the box. Matching names alone are insufficient. Do not create a private lock or install host rules only inside the box."
    } else {
        "Install the supplied host tmpfiles rule and run sudo systemd-tmpfiles --create lianli.conf. For user service setup, add your user to the host lianli group, log out and back in, then restart the selected service. A system service must use the packaged lianli account/group. Do not replace an active lock."
    };
    let mut lock = InstallationFinding {
        code: "runtime.lock_access".into(),
        state: CheckState::Unavailable,
        severity: FindingSeverity::Error,
        feature: "Daemon startup".into(),
        context: label.clone(),
        title: "Process access to the host ownership lock".into(),
        evidence: String::new(),
        remediation: repair.into(),
        guide,
    };
    let mut result = Vec::new();
    match access {
        Ok((required, allowed)) => {
            lock.state = if allowed { CheckState::Passed } else { CheckState::Failed };
            lock.severity = if allowed { FindingSeverity::Info } else { FindingSeverity::Error };
            lock.evidence = format!("Effective UID {uid}. Visible host lock group GID {required}. Read/write permission {}. Kernel access checks include effective credentials and ACLs. The lock was not acquired. This does not prove USB device or security-policy access.", if allowed { "granted" } else { "denied" });
            let mut group = InstallationFinding { code:"runtime.groups".into(), state:CheckState::Passed,
                severity:FindingSeverity::Info, feature:"Process credentials".into(), context:label,
                title:"Numeric group credentials".into(), evidence:format!("Effective process groups: {}. The required lock GID is {required}.", group_summary(groups)),
                remediation:repair.into(), guide };
            if !groups.contains(&required) {
                match configured {
                    Ok(configured) if configured.contains(&required) => {
                        group.state = CheckState::Failed;
                        group.severity = FindingSeverity::Warning;
                        group.title = "Group membership changed after this process started".into();
                        group.evidence.push_str(" This process has not picked up the account's group membership. Restart the login and service. For Distrobox, stop and re-enter the box too.");
                    }
                    Ok(_) if !allowed => {
                        group.state = CheckState::Failed;
                        group.severity = FindingSeverity::Warning;
                        group.title = "The process lacks the host lock's numeric group".into();
                        group.evidence.push_str(" Its account database does not include this visible GID either. Check host group setup and container group mappings.");
                    }
                    Err(error) if !allowed => {
                        group.state = CheckState::Unavailable;
                        group.severity = FindingSeverity::Warning;
                        group.evidence.push_str(&format!(" Configured account groups could not be checked: {error:#}"));
                    }
                    _ => group.evidence.push_str(" Owner permissions or an ACL provide access. No group change is needed."),
                }
            }
            result.push(group);
        }
        Err(error) => lock.evidence = format!("Cannot verify host lock access with this process's credentials: {error:#}. A missing/hidden lock or unavailable kernel access check is not a healthy result."),
    }
    result.push(lock);
    result
}

fn group_summary(groups: &[u32]) -> String {
    let text = groups
        .iter()
        .take(16)
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if groups.len() > 16 {
        format!("{text} ({} total)", groups.len())
    } else {
        text
    }
}

pub fn inspect(hid_backend: Option<HidBackend>) -> Result<Report> {
    let context = InstallationContext::detect();
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let groups = crate::account::current_groups(gid)?;
    let configured = crate::account::Account::user(uid).map(|account| account.groups);
    let access = context
        .daemon_lock_path()
        .ok_or_else(|| anyhow::anyhow!("The container's host lock is unavailable"))
        .and_then(|path| access(&path));
    let mut findings = findings(&context, uid, &groups, configured, access);
    if let Some(backend) = hid_backend {
        findings.push(crate::usb_permissions::inspect(&context, backend));
    } else if let Some(finding) = hermes_access(uid) {
        findings.push(finding);
    }
    Ok(Report {
        version: env!("CARGO_PKG_VERSION").into(),
        uid,
        gid,
        groups_fingerprint: group_fingerprint(&groups),
        hid_backend,
        findings,
    })
}

fn hermes_access(uid: u32) -> Option<InstallationFinding> {
    hermes_access_at(Path::new("/sys/class/drm"), Path::new("/dev/dri"), uid)
}

fn hermes_access_at(sysfs: &Path, nodes: &Path, uid: u32) -> Option<InstallationFinding> {
    use std::os::unix::fs::FileTypeExt;

    let entries = std::fs::read_dir(sysfs).ok()?;
    let mut denied = Vec::new();
    let mut checked = 0;
    let mut unknown = Vec::new();
    for entry in entries.take(256).flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.strip_prefix("renderD").is_some_and(|index| {
            !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            continue;
        }
        let role = std::fs::read_to_string(entry.path().join("device/hermes_kms_role"));
        if !role.is_ok_and(|role| role.trim() == "host") {
            continue;
        }
        let path = nodes.join(name.as_ref());
        match std::fs::read_to_string(entry.path().join("device/hermes_kms_access_uid")) {
            Ok(owner) if !owner.trim().is_empty() => continue,
            Ok(_) => {}
            Err(error) => {
                unknown.push(format!(
                    "{}: cannot verify Hermes ownership: {error}",
                    path.display()
                ));
                continue;
            }
        }
        let access = (|| -> Result<bool> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)?;
            ensure!(
                file.metadata()?.file_type().is_char_device(),
                "Not a DRM device node"
            );
            effective_access(&file)
        })();
        match access {
            Ok(true) => checked += 1,
            Ok(false) => denied.push(path.display().to_string()),
            Err(error) => unknown.push(format!("{}: {error:#}", path.display())),
        }
    }
    if checked == 0 && denied.is_empty() && unknown.is_empty() {
        return None;
    }
    denied.sort();
    unknown.sort();
    let (state, evidence) = if !denied.is_empty() {
        (
            CheckState::Failed,
            format!(
                "Read/write access denied: {}. Effective UID {uid}, including ACL masks.",
                denied.join(", ")
            ),
        )
    } else if !unknown.is_empty() {
        (CheckState::Unavailable, unknown.join(". "))
    } else {
        (CheckState::Passed, format!("Read/write permission granted for {checked} host render node(s). Capture compatibility is checked at startup."))
    };
    Some(InstallationFinding {
        code: "runtime.hermes_access".into(),
        state,
        severity: if state == CheckState::Passed {
            FindingSeverity::Info
        } else {
            FindingSeverity::Warning
        },
        feature: "Hermes desktop capture".into(),
        context: format!("Process UID {uid}"),
        title: "Hermes host render access".into(),
        evidence,
        remediation: if state == CheckState::Passed {
            String::new()
        } else {
            "Install the current 60-lianli.rules on the host, run sudo udevadm control --reload-rules, then reboot. It includes the Hermes host ACL fix. Distrobox-only installation is insufficient. If access still fails, follow the Hermes permissions section in the setup guide.".into()
        },
        guide: InstallationGuide::Troubleshooting,
    })
}

fn group_fingerprint(groups: &[u32]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for gid in groups {
        hash.update(gid.to_le_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub fn collect(executable: &Path) -> Result<Vec<InstallationFinding>> {
    Ok(collect_report(executable, None)?.findings)
}

pub fn collect_report(executable: &Path, hid_backend: Option<HidBackend>) -> Result<Report> {
    collect_report_with_media(executable, hid_backend, false, None)
}

pub fn collect_report_with_media(
    executable: &Path,
    hid_backend: Option<HidBackend>,
    media_tools: bool,
    state_directory: Option<&Path>,
) -> Result<Report> {
    let executable = executable.to_owned();
    let state_directory = state_directory.map(Path::to_path_buf);
    run_check(
        move || {
            collect_report_inner(
                &executable,
                hid_backend,
                media_tools,
                state_directory.as_deref(),
            )
        },
        Duration::from_millis(3500),
    )
}

pub(crate) fn run_check<T: Send + 'static>(
    job: impl FnOnce() -> Result<T> + Send + 'static,
    timeout: Duration,
) -> Result<T> {
    ensure!(
        RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "A runtime diagnostic worker is still running. Repair unavailable storage and Recheck"
    );
    let running = Running;
    let (sender, receiver) = mpsc::sync_channel(1);
    // A killed helper can remain in uninterruptible I/O. Its worker retains the only slot
    // until reaping finishes, without blocking the IPC caller or daemon shutdown.
    std::thread::Builder::new()
        .name("runtime-health".into())
        .spawn(move || {
            let _running = running;
            let _ = sender.send(job());
        })?;
    receiver.recv_timeout(timeout).context("Runtime checks did not finish. Storage or account services may be unavailable. Repair them and Recheck")?
}

fn collect_report_inner(
    executable: &Path,
    hid_backend: Option<HidBackend>,
    media_tools: bool,
    state_directory: Option<&Path>,
) -> Result<Report> {
    let mut command = std::process::Command::new(executable);
    command.arg("inspect-runtime");
    if media_tools {
        command.arg("--media-tools");
    }
    if let Some(path) = state_directory {
        command.arg("--state-directory").arg(path);
    }
    if let Some(backend) = hid_backend {
        command.arg("--hid-backend").arg(backend.to_string());
    }
    let output = crate::command::run(command, std::time::Duration::from_secs(3))?;
    ensure!(
        output.status.success(),
        "Runtime helper failed: {}",
        output.stderr.trim()
    );
    let report: Report = serde_json::from_str(&output.stdout)?;
    let gid = unsafe { libc::getegid() };
    ensure!(
        report.version == env!("CARGO_PKG_VERSION")
            && report.hid_backend == hid_backend
            && report.uid == unsafe { libc::geteuid() }
            && report.gid == gid
            && report.groups_fingerprint
                == group_fingerprint(&crate::account::current_groups(gid)?),
        "The runtime helper version or inherited account credentials differ from this process"
    );
    ensure!(
        report.findings.len() <= 16,
        "Runtime helper returned too many findings"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn timed_out_runtime_checks_retain_the_slot_until_the_worker_exits() {
        let (release, blocked) = mpsc::sync_channel(1);
        let began = std::time::Instant::now();
        let error = run_check(
            move || {
                blocked.recv()?;
                Ok(())
            },
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not finish"));
        assert!(began.elapsed() < Duration::from_secs(2));
        assert!(run_check(
            || -> Result<()> { panic!("Do not spawn another check") },
            Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("still running"));
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while RUNNING.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(run_check(|| Ok(()), Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn hermes_access_excludes_private_sessions_and_rejects_missing_or_replaced_nodes() {
        let root = tempfile::tempdir().unwrap();
        let sysfs = root.path().join("sys");
        let nodes = root.path().join("dev");
        let device = sysfs.join("renderD130/device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::create_dir(&nodes).unwrap();
        let role = device.join("hermes_kms_role");
        let owner = device.join("hermes_kms_access_uid");
        std::fs::write(&owner, "").unwrap();
        std::fs::write(&role, "session\n").unwrap();
        assert!(hermes_access_at(&sysfs, &nodes, 1000).is_none());
        std::fs::write(&role, "host\n").unwrap();
        let missing = hermes_access_at(&sysfs, &nodes, 1000).unwrap();
        assert_eq!(missing.state, CheckState::Unavailable);
        assert!(missing.evidence.contains("renderD130"));
        std::fs::write(nodes.join("renderD130"), "replacement").unwrap();
        let replaced = hermes_access_at(&sysfs, &nodes, 1000).unwrap();
        assert_eq!(replaced.state, CheckState::Unavailable);
        assert!(replaced.evidence.contains("Not a DRM device node"));
        std::fs::write(&owner, "2000\n").unwrap();
        assert!(hermes_access_at(&sysfs, &nodes, 1000).is_none());
    }

    #[test]
    fn stale_groups_are_distinct_from_custom_access_and_container_mapping_failures() {
        let native = InstallationContext::Native;
        let stale = findings(
            &native,
            1000,
            &[1000],
            Ok(vec![500, 1000]),
            Ok((500, false)),
        );
        assert_eq!(
            stale[0].title,
            "Group membership changed after this process started"
        );
        assert_eq!(stale[1].state, CheckState::Failed);
        let custom = findings(&native, 1000, &[1000], Ok(vec![1000]), Ok((500, true)));
        assert!(custom
            .iter()
            .all(|finding| finding.state == CheckState::Passed));
        let container = InstallationContext::Distrobox {
            name: "a box".into(),
        };
        let missing = findings(
            &container,
            1000,
            &[501, 1000],
            Ok(vec![501, 1000]),
            Ok((500, false)),
        );
        assert_eq!(missing[0].state, CheckState::Failed);
        assert!(missing[0].context.contains("a box"));
        assert!(missing[0].remediation.contains("numeric group IDs"));
        let unavailable = findings(
            &native,
            1000,
            &[1000],
            Err(anyhow::anyhow!("NSS unavailable")),
            Ok((500, false)),
        );
        assert_eq!(unavailable[0].state, CheckState::Unavailable);
        let absent = findings(
            &native,
            1000,
            &[1000],
            Ok(vec![1000]),
            Err(anyhow::anyhow!("missing lock")),
        );
        assert_eq!(absent.len(), 1);
        assert_eq!(absent[0].state, CheckState::Unavailable);
    }

    #[test]
    fn access_check_does_not_create_write_or_lock_its_target() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        assert!(access(&path).is_err());
        assert!(!path.exists());
        fs::write(&path, b"retained owner text").unwrap();
        assert!(access(&path).unwrap().1);
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            assert!(!access(&path).unwrap().1);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), b"retained owner text");
        let file = fs::File::open(&path).unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(access(&path).unwrap().1);
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(access(&alias).is_err());
        assert!(access(root.path()).is_err());
    }
}
