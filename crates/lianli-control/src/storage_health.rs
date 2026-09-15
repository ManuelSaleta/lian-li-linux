use anyhow::{ensure, Context, Result};
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationFinding, InstallationGuide,
};
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn inspect(path: &Path, temporary: bool) -> InstallationFinding {
    let result = check_location(path, !temporary);
    let (state, evidence) = match result {
        Ok((available, missing)) => (CheckState::Passed, format!("{}Write/search access is available with {available} bytes free. No write was attempted. Quotas or later changes may prevent saving.", if missing { "The state directory will be created on first save. " } else { "" })),
        Err(error) => (CheckState::Failed, format!("{}: {error:#}", path.display())),
    };
    InstallationFinding {
        code: if temporary { "storage.media_temp" } else { "storage.state" }.into(),
        state, severity: if state == CheckState::Passed { FindingSeverity::Info } else { FindingSeverity::Warning },
        feature: if temporary { "Media preparation" } else { "Settings persistence" }.into(),
        context: "Selected daemon".into(),
        title: if temporary { "Temporary media storage" } else { "State directory storage" }.into(),
        evidence,
        remediation: "Check the daemon account's directory access, mount state, free space and quotas in its filesystem namespace. Create a missing state directory with the intended daemon ownership using the installation guide. Repair storage, Recheck, then retry the failed save or media preparation.".into(),
        guide: InstallationGuide::Troubleshooting,
    }
}

fn check_location(path: &Path, create_parents: bool) -> Result<(u64, bool)> {
    let mut candidate = path;
    for depth in 0..128 {
        match check(candidate) {
            Ok(available) => return Ok((available, depth > 0)),
            Err(error)
                if create_parents
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                match std::fs::symlink_metadata(candidate) {
                    Ok(_) => return Err(error),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                candidate = candidate
                    .parent()
                    .context("No accessible state directory ancestor")?;
                if candidate.as_os_str().is_empty() {
                    candidate = Path::new(".");
                }
            }
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!("State directory exceeds the ancestor lookup limit")
}

fn check(path: &Path) -> Result<u64> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
        .context("Opening storage directory")?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            directory.as_raw_fd(),
            c"".as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EMPTY_PATH | libc::AT_EACCESS,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("Checking effective write/search access");
    }
    let pinned = std::ffi::CString::new(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
    let mut statistics = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(pinned.as_ptr(), statistics.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Reading storage capacity");
    }
    let statistics = unsafe { statistics.assume_init() };
    ensure!(
        statistics.f_flag & libc::ST_RDONLY == 0,
        "Filesystem is read-only"
    );
    let available = statistics.f_bavail.saturating_mul(statistics.f_frsize);
    ensure!(
        available > 0,
        "No free storage is available to this account"
    );
    ensure!(
        statistics.f_files == 0 || statistics.f_favail > 0,
        "No free file entries are available to this account"
    );
    Ok(available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_inspection_does_not_create_files_or_accept_non_directories() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(inspect(root.path(), false).state, CheckState::Passed);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let missing = root.path().join("missing");
        assert_eq!(inspect(&missing, false).state, CheckState::Passed);
        assert_eq!(inspect(&missing, true).state, CheckState::Failed);
        assert!(!missing.exists());
        let link = root.path().join("dangling");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        assert_eq!(
            inspect(&link.join("state"), false).state,
            CheckState::Failed
        );
        let file = root.path().join("file");
        std::fs::write(&file, "preserve").unwrap();
        assert_eq!(inspect(&file, true).state, CheckState::Failed);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "preserve");
    }
}
