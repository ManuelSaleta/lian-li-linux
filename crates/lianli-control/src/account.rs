use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub name: String,
    pub home: PathBuf,
}

impl Account {
    pub fn user(uid: u32) -> Result<Self> {
        Self::lookup(Some(uid))
    }

    pub fn system() -> Result<Self> {
        Self::lookup(None)
    }

    pub fn authorized_caller() -> Result<Self> {
        ensure!(
            unsafe { libc::geteuid() } == 0,
            "Service switching requires administrator authorization"
        );
        let uid: u32 = std::env::var("PKEXEC_UID")
            .context("Start automatic service switching from the native GUI authorization flow")?
            .parse()
            .context("Invalid authorized caller identity")?;
        Self::user(uid)
    }

    fn lookup(uid: Option<u32>) -> Result<Self> {
        let mut bytes = vec![0u8; 16 * 1024];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        let result = unsafe {
            if let Some(uid) = uid {
                libc::getpwuid_r(
                    uid,
                    entry.as_mut_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    &mut found,
                )
            } else {
                libc::getpwnam_r(
                    c"lianli".as_ptr(),
                    entry.as_mut_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    &mut found,
                )
            }
        };
        ensure!(
            result == 0 && !found.is_null(),
            "Daemon account is unavailable. Check the user or installed sysusers rule"
        );
        let entry = unsafe { entry.assume_init() };
        ensure!(
            entry.pw_uid != 0,
            "Hardware operations require an unprivileged daemon account"
        );
        let name = unsafe { CStr::from_ptr(entry.pw_name) }
            .to_str()?
            .to_string();
        let home = PathBuf::from(unsafe { CStr::from_ptr(entry.pw_dir) }.to_str()?);
        ensure!(
            !name.is_empty() && name.len() <= 256 && home.is_absolute(),
            "Invalid daemon account name or home directory"
        );
        let name_c = CString::new(name.as_bytes())?;
        let gid = if uid.is_none() {
            system_group()?
        } else {
            entry.pw_gid
        };
        let mut groups = vec![0; 64];
        let mut count = groups.len() as i32;
        let mut result =
            unsafe { libc::getgrouplist(name_c.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
        if result < 0 {
            ensure!(
                (1..=65536).contains(&count),
                "Daemon supplementary group list exceeds its limit"
            );
            groups.resize(count as usize, 0);
            result = unsafe {
                libc::getgrouplist(name_c.as_ptr(), gid, groups.as_mut_ptr(), &mut count)
            };
        }
        ensure!(
            result >= 0 && count >= 0 && count as usize <= groups.len(),
            "Cannot resolve daemon supplementary groups"
        );
        groups.truncate(count as usize);
        groups.sort_unstable();
        groups.dedup();
        Ok(Self {
            uid: entry.pw_uid,
            gid,
            groups,
            name,
            home,
        })
    }

    pub fn control_command(&self, args: &[&OsStr]) -> Result<Command> {
        self.command("/proc/self/exe", args)
    }

    pub(crate) fn user_service_command(
        &self,
        action: lianli_shared::services::ServiceAction,
    ) -> Result<Command> {
        self.command(
            "/usr/bin/systemctl",
            &[
                OsStr::new("--user"),
                OsStr::new("--no-ask-password"),
                OsStr::new("--no-pager"),
                OsStr::new("--no-block"),
                OsStr::new("--job-mode=fail"),
                OsStr::new(action.argument()),
                OsStr::new(lianli_shared::services::USER_UNIT),
            ],
        )
    }

    pub fn verify_current(&self) -> Result<()> {
        ensure!(self.uid == unsafe { libc::geteuid() } && self.gid == unsafe { libc::getegid() }
            && self.groups == current_groups(self.gid)?,
            "Destination helper does not have the intended UID, primary group and supplementary groups");
        Ok(())
    }

    pub fn group_fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for gid in &self.groups {
            hash.update(gid.to_le_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    pub(crate) fn command(&self, program: &str, args: &[&OsStr]) -> Result<Command> {
        ensure!(self.uid != 0, "Refusing to run an account helper as root");
        let root = unsafe { libc::geteuid() } == 0;
        if !root {
            self.verify_current()?;
        }
        let mut command = Command::new(program);
        let runtime = format!("/run/user/{}", self.uid);
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("USER", &self.name)
            .env("LOGNAME", &self.name)
            .env("LC_ALL", "C")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={runtime}/bus"),
            )
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                "unix:path=/run/dbus/system_bus_socket",
            )
            .current_dir("/");
        let uid = self.uid;
        let gid = self.gid;
        let groups = self.groups.clone();
        unsafe {
            command.pre_exec(move || {
                if root
                    && (libc::setgroups(groups.len(), groups.as_ptr()) != 0
                        || libc::setresgid(gid, gid, gid) != 0
                        || libc::setresuid(uid, uid, uid) != 0)
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(command)
    }
}

pub(crate) fn current_groups(gid: u32) -> Result<Vec<u32>> {
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    ensure!(
        (0..=65536).contains(&count),
        "Cannot inspect current supplementary groups"
    );
    let mut groups = vec![0; count as usize];
    ensure!(
        unsafe { libc::getgroups(count, groups.as_mut_ptr()) } == count,
        "Supplementary groups changed while preparing the helper"
    );
    groups.push(gid);
    groups.sort_unstable();
    groups.dedup();
    Ok(groups)
}

fn system_group() -> Result<u32> {
    let mut bytes = vec![0u8; 16 * 1024];
    let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut found = std::ptr::null_mut();
    let result = unsafe {
        libc::getgrnam_r(
            c"lianli".as_ptr(),
            entry.as_mut_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut found,
        )
    };
    ensure!(
        result == 0 && !found.is_null(),
        "Packaged lianli service group is unavailable"
    );
    Ok(unsafe { entry.assume_init().gr_gid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_service_actions_use_only_the_callers_manager_and_fixed_unit() {
        let account = Account::user(if unsafe { libc::geteuid() } == 0 {
            65534
        } else {
            unsafe { libc::geteuid() }
        })
        .unwrap();
        for action in [
            lianli_shared::services::ServiceAction::Start,
            lianli_shared::services::ServiceAction::Stop,
            lianli_shared::services::ServiceAction::Restart,
        ] {
            let command = account.user_service_command(action).unwrap();
            assert_eq!(command.get_program(), OsStr::new("/usr/bin/systemctl"));
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    "--user",
                    "--no-ask-password",
                    "--no-pager",
                    "--no-block",
                    "--job-mode=fail",
                    action.argument(),
                    lianli_shared::services::USER_UNIT,
                ]
                .map(OsStr::new)
            );
            let environment: std::collections::HashMap<_, _> = command.get_envs().collect();
            let runtime = format!("/run/user/{}", account.uid);
            let bus = format!("unix:path={runtime}/bus");
            assert_eq!(
                environment.get(OsStr::new("XDG_RUNTIME_DIR")),
                Some(&Some(OsStr::new(&runtime)))
            );
            assert_eq!(
                environment.get(OsStr::new("DBUS_SESSION_BUS_ADDRESS")),
                Some(&Some(OsStr::new(&bus)))
            );
            assert_eq!(command.get_current_dir(), Some(std::path::Path::new("/")));
        }
    }

    #[test]
    fn self_exec_survives_the_account_change() {
        if let Ok(expected) = std::env::var("LIANLI_ACCOUNT_TEST_UID") {
            assert_eq!(unsafe { libc::geteuid() }, expected.parse::<u32>().unwrap());
            assert_ne!(unsafe { libc::geteuid() }, 0);
            return;
        }
        let root = unsafe { libc::geteuid() } == 0;
        let uid = if root {
            65534
        } else {
            unsafe { libc::geteuid() }
        };
        let gid = if root {
            65534
        } else {
            unsafe { libc::getegid() }
        };
        let groups = if root {
            vec![gid]
        } else {
            current_groups(gid).unwrap()
        };
        let account = Account {
            uid,
            gid,
            groups,
            name: "fixture".into(),
            home: "/fixture".into(),
        };
        let mut command = account
            .control_command(&[
                OsStr::new("account::tests::self_exec_survives_the_account_change"),
                OsStr::new("--exact"),
            ])
            .unwrap();
        command.env("LIANLI_ACCOUNT_TEST_UID", uid.to_string());
        let result = crate::command::run(command, std::time::Duration::from_secs(3)).unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            result.stdout,
            result.stderr
        );
        assert!(result.stdout.contains("1 passed"));
    }

    #[test]
    fn child_runs_with_exact_credentials_no_new_privileges_and_a_clean_environment() {
        let root = unsafe { libc::geteuid() } == 0;
        let uid = if root {
            65534
        } else {
            unsafe { libc::geteuid() }
        };
        let gid = if root {
            65534
        } else {
            unsafe { libc::getegid() }
        };
        let groups = if root {
            vec![gid]
        } else {
            current_groups(gid).unwrap()
        };
        let account = Account {
            uid,
            gid,
            groups: groups.clone(),
            name: "fixture".into(),
            home: "/fixture".into(),
        };
        let command = account
            .command("/usr/bin/cat", &[OsStr::new("/proc/self/status")])
            .unwrap();
        let result = crate::command::run(command, std::time::Duration::from_secs(3)).unwrap();
        assert!(result.status.success());
        let status: std::collections::HashMap<_, _> = result
            .stdout
            .lines()
            .filter_map(|line| line.split_once(':'))
            .collect();
        let numbers = |key| {
            status[key]
                .split_whitespace()
                .map(|value| value.parse::<u32>().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(numbers("Uid"), vec![uid; 4]);
        assert_eq!(numbers("Gid"), vec![gid; 4]);
        assert_eq!(numbers("NoNewPrivs"), vec![1]);
        let mut actual = numbers("Groups");
        actual.push(gid);
        actual.sort_unstable();
        actual.dedup();
        assert_eq!(actual, groups);
        let command = account.command("/usr/bin/env", &[]).unwrap();
        let expected = command.get_envs().count();
        let result = crate::command::run(command, std::time::Duration::from_secs(3)).unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout.lines().count(), expected);
        assert!(result.stdout.lines().any(|line| line == "HOME=/fixture"));
    }

    #[test]
    fn account_helpers_clear_inherited_environment_and_use_exact_arguments() {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let account = Account {
            uid: if uid == 0 { 65534 } else { uid },
            gid,
            groups: current_groups(gid).unwrap(),
            name: "fixture".into(),
            home: "/fixture home".into(),
        };
        let command = account
            .control_command(&[
                OsStr::new("inspect-state"),
                OsStr::new("/a path/$(literal)"),
            ])
            .unwrap();
        assert_eq!(command.get_program(), "/proc/self/exe");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["inspect-state", "/a path/$(literal)"]
        );
        let environment: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            environment.get(OsStr::new("HOME")),
            Some(&Some(OsStr::new("/fixture home")))
        );
        assert!(!environment.contains_key(OsStr::new("LD_PRELOAD")));
        assert_eq!(command.get_current_dir(), Some(std::path::Path::new("/")));
        let mut root = account;
        root.uid = 0;
        assert!(root.control_command(&[]).is_err());
    }
}
