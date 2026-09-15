fn run_as_desktop_user(test: &str) -> bool {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    use std::time::{Duration, Instant};
    if unsafe { libc::geteuid() } != 0 {
        return false;
    }
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("private-tests");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::chown(directory.path(), Some(65534), Some(65534)).unwrap();
    let mut command = std::process::Command::new(binary);
    command.args(["--exact", test]).env("TMPDIR", directory.path());
    // Drop inherited root credentials using only async-signal-safe syscalls before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(65534) != 0
                || libc::setuid(65534) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "Unprivileged test failed: {test}");
            return true;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Unprivileged test timed out: {test}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
