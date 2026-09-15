use std::io;

const MAX_ADDRESS_SPACE: libc::rlim_t = 2 * 1024 * 1024 * 1024;

pub(super) fn apply() -> io::Result<()> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    limits.rlim_max = limits.rlim_max.min(MAX_ADDRESS_SPACE);
    limits.rlim_cur = limits.rlim_cur.min(limits.rlim_max);
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    fn observed_limits(inherited: Option<libc::rlimit>) -> Vec<u64> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "ulimit -Sv; ulimit -Hv"]);
        unsafe {
            command.pre_exec(move || {
                if let Some(limits) = inherited {
                    if libc::setrlimit(libc::RLIMIT_AS, &limits) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                apply()
            });
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }

    #[test]
    fn transfer_worker_has_a_hard_address_space_ceiling() {
        let limits = observed_limits(None);
        assert_eq!(limits.len(), 2);
        assert!(limits[0] <= limits[1]);
        assert!(limits[1] <= MAX_ADDRESS_SPACE / 1024);
    }

    #[test]
    fn transfer_worker_preserves_stricter_inherited_limits() {
        let limits = observed_limits(Some(libc::rlimit {
            rlim_cur: 32 * 1024 * 1024,
            rlim_max: 64 * 1024 * 1024,
        }));
        assert_eq!(limits, [32 * 1024, 64 * 1024]);
    }
}
