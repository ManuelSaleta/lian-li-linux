pub fn start() {
    let executable = match std::env::current_exe() {
        Ok(path) => path.with_file_name("lianli-session"),
        Err(error) => {
            tracing::warn!("Cannot locate the desktop session worker: {error}");
            return;
        }
    };
    // The helper outlives the GUI; this thread only reaps it while the GUI remains open.
    if let Err(error) = std::thread::Builder::new()
        .name("session-launch".into())
        .spawn(move || {
            if executable == std::path::Path::new("/usr/bin/lianli-session")
                && std::path::Path::new("/usr/lib/systemd/user/lianli-session.service").is_file()
                && matches!(
                    lianli_shared::installation::InstallationContext::detect(),
                    lianli_shared::installation::InstallationContext::Native
                )
            {
                let mut command = std::process::Command::new("systemctl");
                command.args(["--user", "start", "--no-block", "lianli-session.service"]);
                match lianli_control::command::run(command, std::time::Duration::from_secs(3)) {
                    Ok(output) if output.status.success() => {}
                    Ok(_) => tracing::warn!(
                        "Desktop login service could not start. Inspect its user-service status"
                    ),
                    Err(error) => tracing::warn!("Desktop login service is unavailable: {error}"),
                }
                return;
            }
            match std::process::Command::new(executable)
                .stdin(std::process::Stdio::null())
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(status) => tracing::warn!("Desktop session worker exited with {status}"),
                Err(error) => tracing::warn!("Desktop session worker could not start: {error}"),
            }
        })
    {
        tracing::warn!("Desktop session launcher could not start: {error}");
    }
}
