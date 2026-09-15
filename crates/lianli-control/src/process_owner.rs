use crate::services::Route;
use anyhow::{ensure, Context, Result};
use lianli_shared::services::{OwnerProcess, ServiceProbe, ServiceScope, UnitState};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

pub fn inspect(
    route: &Route,
    pid: u32,
    user: &ServiceProbe<UnitState>,
    system: &ServiceProbe<UnitState>,
) -> Result<OwnerProcess> {
    ensure!(pid > 0, "Invalid hardware owner PID");
    let mut process = read_process(pid, |name| read_file(route, pid, name))?;
    process.service = classify(&process, user, system, unsafe { libc::geteuid() })?;
    Ok(process)
}

fn read_file(route: &Route, pid: u32, name: &str) -> Result<String> {
    let path = format!("/proc/{pid}/{name}");
    match route {
        Route::Native => {
            let mut data = String::new();
            File::open(&path)?
                .take(64 * 1024 + 1)
                .read_to_string(&mut data)?;
            ensure!(data.len() <= 64 * 1024, "Process metadata exceeds 64 KiB");
            Ok(data)
        }
        Route::Host { .. } => {
            let output = route.output("/usr/bin/cat", &["--", &path])?;
            ensure!(
                output.status.success(),
                "Cannot read host process {name}: {}",
                output.stderr.trim()
            );
            Ok(output.stdout)
        }
    }
}

pub(crate) fn verify_container_peer(
    route: &Route,
    owner: &OwnerProcess,
    peer_pid: i32,
) -> Result<()> {
    ensure!(
        peer_pid > 0,
        "Container IPC peer is not visible in this PID namespace"
    );
    let status = read_file(route, owner.pid, "status")?;
    ensure!(
        parse_uid(&status)? == owner.effective_uid,
        "Host process credentials changed"
    );
    let inner_pid = namespace_pid(&status)?;
    ensure!(
        inner_pid == peer_pid as u32,
        "Container IPC peer does not match the host owner's namespace PID"
    );
    let path = format!("/proc/{}/ns/pid", owner.pid);
    let output = route.output(
        "/usr/bin/stat",
        &["--dereference", "--format=%d:%i", "--", &path],
    )?;
    ensure!(
        output.status.success(),
        "Cannot verify the host owner's PID namespace: {}",
        output.stderr.trim()
    );
    let local = std::fs::metadata(format!("/proc/{peer_pid}/ns/pid"))?;
    ensure!(
        output.stdout.trim() == format!("{}:{}", local.dev(), local.ino()),
        "Host owner and IPC peer belong to different PID namespaces"
    );
    let local_start = parse_start_time(
        &read_file(&Route::Native, peer_pid as u32, "stat")?,
        peer_pid as u32,
    )?;
    let host_start = parse_start_time(&read_file(route, owner.pid, "stat")?, owner.pid)?;
    ensure!(
        host_start == local_start && host_start.to_string() == owner.start_time_ticks,
        "Host owner or container IPC process changed during verification"
    );
    Ok(())
}

fn namespace_pid(status: &str) -> Result<u32> {
    let mut lines = status
        .lines()
        .filter_map(|line| line.strip_prefix("NSpid:"));
    let values = lines
        .next()
        .context("Host process namespace PID is unavailable")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        lines.next().is_none()
            && !values.is_empty()
            && values.len() <= 32
            && values.iter().all(|pid| *pid > 0),
        "Invalid process namespace PID hierarchy"
    );
    Ok(*values.last().unwrap())
}

fn read_process(pid: u32, mut read: impl FnMut(&str) -> Result<String>) -> Result<OwnerProcess> {
    let start_time = parse_start_time(&read("stat")?, pid)?;
    let effective_uid = parse_uid(&read("status")?)?;
    let control_group = parse_cgroup(&read("cgroup")?)?;
    ensure!(
        parse_start_time(&read("stat")?, pid)? == start_time,
        "Hardware owner PID was reused during inspection"
    );
    Ok(OwnerProcess {
        pid,
        effective_uid,
        start_time_ticks: start_time.to_string(),
        control_group,
        service: None,
    })
}

fn parse_start_time(text: &str, expected_pid: u32) -> Result<u64> {
    let (pid, rest) = text
        .split_once(" (")
        .context("Invalid process stat prefix")?;
    ensure!(
        pid.parse::<u32>()? == expected_pid,
        "Process stat PID mismatch"
    );
    // comm may contain spaces, newlines and parentheses; numeric fields follow its last closing parenthesis.
    let (_, fields) = rest
        .rsplit_once(") ")
        .context("Invalid process stat name")?;
    fields
        .split_whitespace()
        .nth(19)
        .context("Process start time is missing")?
        .parse()
        .context("Invalid process start time")
}

fn parse_uid(text: &str) -> Result<u32> {
    let mut lines = text.lines().filter_map(|line| line.strip_prefix("Uid:"));
    let fields: Vec<_> = lines
        .next()
        .context("Process credentials are missing")?
        .split_whitespace()
        .collect();
    ensure!(
        fields.len() == 4 && lines.next().is_none(),
        "Ambiguous process credentials"
    );
    let values = fields
        .into_iter()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(values[1])
}

fn valid_group(path: &str) -> bool {
    path == "/"
        || (path.starts_with('/')
            && path.len() <= 4096
            && path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && !path.bytes().any(|byte| byte.is_ascii_control()))
}

fn parse_cgroup(text: &str) -> Result<String> {
    let mut unified = None;
    let mut legacy = None;
    for line in text.lines() {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields
            .next()
            .context("Missing cgroup hierarchy")?
            .parse::<u32>()?;
        let controllers = fields.next().context("Missing cgroup controllers")?;
        let path = fields.next().context("Missing cgroup path")?;
        ensure!(valid_group(path), "Invalid process cgroup path");
        let slot = if hierarchy == 0 && controllers.is_empty() {
            &mut unified
        } else if controllers.split(',').any(|name| name == "name=systemd") {
            &mut legacy
        } else {
            continue;
        };
        ensure!(
            slot.replace(path).is_none(),
            "Ambiguous process cgroup hierarchy"
        );
    }
    legacy
        .or(unified)
        .map(str::to_owned)
        .context("No systemd process cgroup is visible")
}

fn belongs_to(process: &OwnerProcess, probe: &ServiceProbe<UnitState>) -> Result<bool> {
    let unit = match probe {
        ServiceProbe::Known { value } => value,
        ServiceProbe::Unavailable { reason } => {
            anyhow::bail!("Service ownership is unverified: {reason}")
        }
    };
    if unit.load_state == "not-found" {
        return Ok(false);
    }
    let group = unit
        .control_group
        .as_deref()
        .context("Service cgroup is unverified")?;
    if group.is_empty() {
        return Ok(false);
    }
    ensure!(
        valid_group(group) && group != "/",
        "Invalid service cgroup path"
    );
    Ok(process.control_group == group
        || process
            .control_group
            .strip_prefix(group)
            .is_some_and(|suffix| suffix.starts_with('/')))
}

fn classify(
    process: &OwnerProcess,
    user: &ServiceProbe<UnitState>,
    system: &ServiceProbe<UnitState>,
    caller_uid: u32,
) -> Result<Option<ServiceScope>> {
    let system_matches = belongs_to(process, system)?;
    if system_matches && matches!(user, ServiceProbe::Unavailable { .. }) {
        if let ServiceProbe::Known { value } = system {
            // A user manager cannot own a process in this system-manager service cgroup.
            if value.name == "lianli-daemon-system.service"
                && value.control_group.as_deref()
                    == Some("/system.slice/lianli-daemon-system.service")
            {
                return Ok(Some(ServiceScope::System));
            }
        }
    }
    let user_matches = belongs_to(process, user)?;
    ensure!(
        !user_matches || !system_matches,
        "Hardware owner matches both service cgroups"
    );
    if user_matches {
        ensure!(
            process.effective_uid == caller_uid,
            "User service owner runs as another account"
        );
        Ok(Some(ServiceScope::User))
    } else if system_matches {
        Ok(Some(ServiceScope::System))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_pids_require_one_bounded_nonzero_hierarchy() {
        assert_eq!(namespace_pid("NSpid:\t123\t456\t7\n").unwrap(), 7);
        for value in [
            "",
            "NSpid:\n",
            "NSpid: 1 0\n",
            "NSpid: 1\nNSpid: 2\n",
            "NSpid: 1 text\n",
        ] {
            assert!(namespace_pid(value).is_err());
        }
    }

    #[test]
    fn namespace_peer_verification_rejects_a_stale_process_snapshot() {
        let pid = std::process::id();
        let mut process = read_process(pid, |name| read_file(&Route::Native, pid, name)).unwrap();
        verify_container_peer(&Route::Native, &process, pid as i32).unwrap();
        process.start_time_ticks = "0".into();
        assert!(verify_container_peer(&Route::Native, &process, pid as i32).is_err());
    }

    #[test]
    fn a_reused_pid_cannot_be_reported_as_the_original_owner() {
        let mut reads = 0;
        let result = read_process(123, |name| {
            Ok(match name {
                "stat" => {
                    reads += 1;
                    let mut fields = vec!["0".to_string(); 20];
                    fields[0] = "S".into();
                    fields[19] = reads.to_string();
                    format!("123 (daemon) {}", fields.join(" "))
                }
                "status" => "Uid: 1000 1000 1000 1000\n".into(),
                "cgroup" => "0::/user.slice/test.service\n".into(),
                _ => panic!("Unexpected process file"),
            })
        });
        assert!(result.unwrap_err().to_string().contains("reused"));
    }

    fn unit(group: &str) -> ServiceProbe<UnitState> {
        ServiceProbe::Known {
            value: UnitState {
                name: "lianli-daemon.service".into(),
                load_state: "loaded".into(),
                active_state: "active".into(),
                sub_state: "running".into(),
                unit_file_state: "enabled".into(),
                main_pid: 100,
                fragment_path: String::new(),
                control_group: Some(group.into()),
                kill_mode: None,
                send_sigkill: None,
                graceful_shutdown: None,
                invocation_id: None,
                distrobox_name: None,
            },
        }
    }

    #[test]
    fn process_start_time_survives_unusual_names_and_keeps_full_precision() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "9007199254740993";
        let stat = format!("123 (name ) \n( weird) {}\n", fields.join(" "));
        assert_eq!(parse_start_time(&stat, 123).unwrap(), 9007199254740993);
        assert!(parse_start_time(&stat, 456).is_err());
        assert!(parse_start_time("123 (broken) S 0", 123).is_err());
        assert_eq!(
            parse_uid("Name:\ttest\nUid:\t1000\t1001\t1002\t1003\n").unwrap(),
            1001
        );
        assert!(parse_uid("Uid: 1 2 3 4\nUid: 1 2 3 4\n").is_err());
    }

    #[test]
    fn supports_unified_and_hybrid_systemd_hierarchies_without_guessing() {
        assert_eq!(
            parse_cgroup("0::/user.slice/a.service\n").unwrap(),
            "/user.slice/a.service"
        );
        assert_eq!(
            parse_cgroup("0::/\n1:name=systemd:/user.slice/a.service\n2:cpu:/other\n").unwrap(),
            "/user.slice/a.service"
        );
        for invalid in [
            "0::/a/../b",
            "0::/a\n0::/b",
            "1:cpu:/a",
            "0::relative",
            "0::/a//b",
        ] {
            assert!(parse_cgroup(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn system_owner_is_verifiable_before_the_user_manager_starts() {
        let user = ServiceProbe::Unavailable {
            reason: "user bus not started".into(),
        };
        let mut system = unit("/system.slice/lianli-daemon-system.service");
        let ServiceProbe::Known { value } = &mut system else {
            unreachable!()
        };
        value.name = "lianli-daemon-system.service".into();
        let mut process = OwnerProcess {
            pid: 222,
            effective_uid: 987,
            start_time_ticks: "456".into(),
            control_group: "/system.slice/lianli-daemon-system.service".into(),
            service: None,
        };
        assert_eq!(
            classify(&process, &user, &system, 1000).unwrap(),
            Some(ServiceScope::System)
        );
        process.control_group.push_str("-other");
        assert!(classify(&process, &user, &system, 1000).is_err());
        process.control_group = "/user.slice/other.service".into();
        assert!(classify(&process, &user, &system, 1000).is_err());
        assert!(classify(&process, &user, &unit("/user.slice/other.service"), 1000).is_err());
    }

    #[test]
    fn attributes_wrapper_children_but_rejects_other_accounts_and_prefix_collisions() {
        let user = unit("/user.slice/1000/app.slice/lianli.service");
        let system = unit("/system.slice/lianli-system.service");
        let mut process = OwnerProcess {
            pid: 222,
            effective_uid: 1000,
            start_time_ticks: "456".into(),
            control_group: "/user.slice/1000/app.slice/lianli.service/container".into(),
            service: None,
        };
        assert_eq!(
            classify(&process, &user, &system, 1000).unwrap(),
            Some(ServiceScope::User)
        );
        assert!(classify(&process, &user, &system, 2000).is_err());
        process.control_group = "/user.slice/1000/app.slice/lianli.service-other".into();
        assert_eq!(classify(&process, &user, &system, 1000).unwrap(), None);
        process.control_group = "/system.slice/lianli-system.service".into();
        process.effective_uid = 950;
        assert_eq!(
            classify(&process, &user, &system, 1000).unwrap(),
            Some(ServiceScope::System)
        );
        assert!(classify(&process, &unit("/"), &system, 1000).is_err());
        assert!(classify(&process, &system, &system, 1000).is_err());
    }
}
