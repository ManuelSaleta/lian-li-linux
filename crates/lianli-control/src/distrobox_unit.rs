use anyhow::{ensure, Context, Result};
use std::path::Path;

pub(super) const WRAPPER_WAIT_SCRIPT: &str = r#"test -z "$1" || exec /usr/bin/timeout 5s /usr/bin/tail --pid="$1" --sleep-interval=0.1 -f /dev/null"#;

pub(crate) fn matches_installed(contents: &str, expected: &str) -> bool {
    contents == expected
        || contents
            == expected.replacen("\nDescription=Lian Li Linux ", "\nDescription=Lian Li ", 1)
}

fn wrapper_wait() -> Result<String> {
    Ok(format!(
        "/usr/bin/sh -c {} -- \"${{MAINPID}}\"",
        quote(WRAPPER_WAIT_SCRIPT)?
    ))
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.- ".contains(&byte))
}

fn quote(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
        "Unit arguments must be nonempty, at most 4096 bytes and contain no control characters"
    );
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}

fn command_prefix(name: &str, host_enter: &Path, guest_binaries: &Path) -> Result<String> {
    ensure!(
        valid_name(name),
        "Invalid Distrobox name. Use its existing container name"
    );
    ensure!(
        host_enter.is_absolute()
            && host_enter
                .file_name()
                .is_some_and(|name| name == "distrobox-enter"),
        "Select the absolute host path to distrobox-enter"
    );
    ensure!(
        guest_binaries.is_absolute(),
        "Select an absolute binary directory inside the box"
    );
    let enter = host_enter
        .to_str()
        .context("Host executable path must be UTF-8")?;
    ensure!(
        !enter.contains('$'),
        "The host executable path cannot contain dollar signs"
    );
    let enter = quote(enter)?;
    let name = quote(name)?;
    // Podman must place the shared box supervisor outside this service's cgroup.
    Ok(format!(
        "/usr/bin/env --unset=INVOCATION_ID {enter} --name {name} --"
    ))
}

pub fn generate(name: &str, host_enter: &Path, guest_binaries: &Path) -> Result<String> {
    let prefix = command_prefix(name, host_enter, guest_binaries)?;
    let wait = wrapper_wait()?;
    let daemon = quote(
        guest_binaries
            .join("lianli-daemon")
            .to_str()
            .context("Guest binary path must be UTF-8")?,
    )?;
    let control = quote(
        guest_binaries
            .join("lianli-control")
            .to_str()
            .context("Guest binary path must be UTF-8")?,
    )?;
    Ok(format!("[Unit]\nDescription=Lian Li Linux Daemon (Distrobox)\nAfter=graphical-session.target\n\n[Service]\nExecStart={prefix} {daemon} --service-invocation ${{INVOCATION_ID}}\nExecStop={prefix} {control} stop-service --invocation-id ${{INVOCATION_ID}}\nExecStop={wait}\nRestart=on-failure\nRestartSec=5s\nKillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=120s\n\n[Install]\nWantedBy=default.target\n"))
}

pub fn generate_session(name: &str, host_enter: &Path, guest_binaries: &Path) -> Result<String> {
    let prefix = command_prefix(name, host_enter, guest_binaries)?;
    let wait = wrapper_wait()?;
    let session = quote(
        guest_binaries
            .join("lianli-session")
            .to_str()
            .context("Guest binary path must be UTF-8")?,
    )?;
    Ok(format!("[Unit]\nDescription=Lian Li Linux desktop capture (Distrobox)\n\n[Service]\nExecStart={prefix} {session} --login-start --service-invocation ${{INVOCATION_ID}}\nExecStop={prefix} {session} --stop-service ${{INVOCATION_ID}}\nExecStop={wait}\nRestart=always\nRestartSec=5s\nKillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=30s\n\n[Install]\nWantedBy=default.target\n"))
}

pub fn generate_managed(
    route: &crate::container_destination::Route,
    scope: lianli_shared::services::ServiceScope,
    owner_uid: u32,
) -> Result<String> {
    use lianli_shared::services::ServiceScope;
    route.validate()?;
    ensure!(
        owner_uid != 0,
        "Container services require an unprivileged owner"
    );
    let launch = &route.launch;
    let prefix = command_prefix(&launch.name, &launch.host_enter, &launch.binaries)?;
    let (config, working) = route.paths(scope);
    let path = |value: &Path| quote(value.to_str().context("Guest paths must be UTF-8")?);
    let working = quote(&format!(
        "--chdir={}",
        working
            .to_str()
            .context("Guest working directory must be UTF-8")?
    ))?;
    let daemon = path(&launch.binaries.join("lianli-daemon"))?;
    let control = path(&launch.binaries.join("lianli-control"))?;
    let config = path(config)?;
    let wait = wrapper_wait()?;
    let (dependencies, account, mode, stop_scope, target) = match scope {
        ServiceScope::User => (
            "After=graphical-session.target\n".into(), String::new(), "", "", "default.target",
        ),
        ServiceScope::System => (
            format!("Requires=user@{owner_uid}.service\nAfter=user@{owner_uid}.service\n"),
            format!("User={owner_uid}\nEnvironment=XDG_RUNTIME_DIR=/run/user/{owner_uid}\nEnvironment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{owner_uid}/bus\nRuntimeDirectory=lianli\nRuntimeDirectoryMode=0755\n"),
            "--system ", "--scope system ", "multi-user.target",
        ),
    };
    Ok(format!("[Unit]\nDescription=Lian Li Linux Daemon (managed Distrobox)\n{dependencies}\n[Service]\n{account}ExecStart={prefix} /usr/bin/env {working} {daemon} {mode}--config {config} --service-invocation ${{INVOCATION_ID}}\nExecStop={prefix} {control} stop-service {stop_scope}--invocation-id ${{INVOCATION_ID}}\nExecStop={wait}\nRestart=on-failure\nRestartSec=5s\nKillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=120s\n\n[Install]\nWantedBy={target}\n"))
}

pub fn generate_system(
    name: &str,
    host_enter: &Path,
    guest_binaries: &Path,
    owner_uid: u32,
    config: &Path,
) -> Result<String> {
    ensure!(
        owner_uid != 0,
        "Select the unprivileged host account that owns the box"
    );
    ensure!(
        config.is_absolute(),
        "Select an absolute configuration path inside the box"
    );
    let config = quote(
        config
            .to_str()
            .context("Configuration path must be UTF-8")?,
    )?;
    let prefix = command_prefix(name, host_enter, guest_binaries)?;
    let wait = wrapper_wait()?;
    let daemon = quote(
        guest_binaries
            .join("lianli-daemon")
            .to_str()
            .context("Guest binary path must be UTF-8")?,
    )?;
    let control = quote(
        guest_binaries
            .join("lianli-control")
            .to_str()
            .context("Guest binary path must be UTF-8")?,
    )?;
    Ok(format!("[Unit]\nDescription=Lian Li Linux Daemon (Distrobox system mode)\nRequires=user@{owner_uid}.service\nAfter=user@{owner_uid}.service\n\n[Service]\nUser={owner_uid}\nEnvironment=XDG_RUNTIME_DIR=/run/user/{owner_uid}\nEnvironment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{owner_uid}/bus\nRuntimeDirectory=lianli\nRuntimeDirectoryMode=0755\nExecStart={prefix} {daemon} --system --config {config} --service-invocation ${{INVOCATION_ID}}\nExecStop={prefix} {control} stop-service --scope system --invocation-id ${{INVOCATION_ID}}\nExecStop={wait}\nRestart=on-failure\nRestartSec=5s\nKillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=120s\n\n[Install]\nWantedBy=multi-user.target\n"))
}

pub fn session_guidance(name: &str) -> Result<String> {
    ensure!(valid_name(name), "Invalid detected Distrobox name");
    Ok(format!("Inside box '{name}', run: lianli-control distrobox-service-unit --desktop-session --box '{name}'. Review the output and install it as ~/.config/systemd/user/lianli-session.service on the host. On the host run systemctl --user daemon-reload, then systemctl --user enable --now lianli-session.service. Keep the host process and private user runtime views shared with the box. See the Distrobox guide for custom paths. This capture helper does not start the hardware daemon."))
}

pub fn guidance(name: &str) -> Result<String> {
    ensure!(valid_name(name), "Invalid detected Distrobox name");
    Ok(format!("Inside box '{name}', run: lianli-control distrobox-service-unit --box '{name}'. It prints the host user unit without installing or starting it. For custom paths, pass --distrobox-enter with the host executable path and --binaries with the guest binary directory. Review the output, stop any older daemon cleanly, then install it as ~/.config/systemd/user/lianli-daemon.service on the host and run systemctl --user daemon-reload there. Follow the Distrobox guide before enabling it."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_modes_pin_guest_working_directories_and_configuration_paths() {
        use crate::container_destination::{Launch, Route};
        use lianli_shared::services::ServiceScope;
        let route = Route {
            launch: Launch {
                name: "box".into(),
                host_enter: "/usr/bin/distrobox-enter".into(),
                binaries: "/usr/bin".into(),
            },
            user_config: "/home/user/user-state/custom.json".into(),
            system_config: "/home/user/system-state/custom.json".into(),
            user_working_directory: "/home/user/$USER %i".into(),
            system_working_directory: "/home/user/system work".into(),
        };
        let user = generate_managed(&route, ServiceScope::User, 1000).unwrap();
        let system = generate_managed(&route, ServiceScope::System, 1000).unwrap();
        assert!(user.contains("/usr/bin/env \"--chdir=/home/user/$$USER %%i\" \"/usr/bin/lianli-daemon\" --config \"/home/user/user-state/custom.json\""));
        assert!(system.contains("/usr/bin/env \"--chdir=/home/user/system work\" \"/usr/bin/lianli-daemon\" --system --config \"/home/user/system-state/custom.json\""));
        assert!(system.contains("User=1000\n"));
        assert!(!user.contains("User=1000\n"));
        for unit in [user, system] {
            assert_eq!(unit.matches("ExecStop=").count(), 2);
            assert!(unit.contains("SendSIGKILL=no\nTimeoutStopSec=120s\n"));
        }
        assert!(generate_managed(&route, ServiceScope::System, 0).is_err());
    }

    #[test]
    fn system_unit_uses_the_box_owner_and_an_explicit_guest_configuration() {
        let unit = generate_system(
            "my box",
            Path::new("/usr/bin/distrobox-enter"),
            Path::new("/opt/app %i"),
            1000,
            Path::new("/home/test/system $USER/config.json"),
        )
        .unwrap();
        assert!(unit.contains("Requires=user@1000.service\nAfter=user@1000.service\n"));
        assert!(unit.contains("User=1000\nEnvironment=XDG_RUNTIME_DIR=/run/user/1000\n"));
        assert!(unit.contains("RuntimeDirectory=lianli\nRuntimeDirectoryMode=0755\n"));
        assert!(unit.contains("\"/opt/app %%i/lianli-daemon\" --system --config \"/home/test/system $$USER/config.json\" --service-invocation ${INVOCATION_ID}\n"));
        assert!(unit.contains("stop-service --scope system --invocation-id ${INVOCATION_ID}\n"));
        assert_eq!(unit.matches("ExecStop=").count(), 2);
        assert!(unit.contains("KillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=120s\n"));
        assert!(unit.ends_with("WantedBy=multi-user.target\n"));
        for (uid, config) in [
            (0, "/config.json"),
            (1000, "relative.json"),
            (1000, "/bad\npath"),
        ] {
            assert!(generate_system(
                "box",
                Path::new("/usr/bin/distrobox-enter"),
                Path::new("/usr/bin"),
                uid,
                Path::new(config)
            )
            .is_err());
        }
    }

    #[test]
    fn session_unit_starts_login_discovery_in_the_selected_box() {
        let unit = generate_session(
            "my box",
            Path::new("/opt/host tools/distrobox-enter"),
            Path::new("/opt/app $USER %i"),
        )
        .unwrap();
        assert!(unit.contains("ExecStart=/usr/bin/env --unset=INVOCATION_ID \"/opt/host tools/distrobox-enter\" --name \"my box\" -- \"/opt/app $$USER %%i/lianli-session\" --login-start --service-invocation ${INVOCATION_ID}\n"));
        assert!(unit.contains("/lianli-session\" --stop-service ${INVOCATION_ID}\n"));
        assert!(!unit.contains("lianli-daemon"));
        assert!(unit.contains("Restart=always\nRestartSec=5s"));
        assert!(unit.ends_with("WantedBy=default.target\n"));
        assert!(generate_session(
            "box\nother",
            Path::new("/usr/bin/distrobox-enter"),
            Path::new("/usr/bin")
        )
        .is_err());
    }

    #[test]
    fn unit_preserves_literal_paths_and_the_invocation_variable() {
        let unit = generate(
            "my box",
            Path::new("/opt/host tools/distrobox-enter"),
            Path::new("/opt/app \"quoted\" \\ $USER %i"),
        )
        .unwrap();
        assert!(unit.contains("ExecStart=/usr/bin/env --unset=INVOCATION_ID \"/opt/host tools/distrobox-enter\" --name \"my box\" -- \"/opt/app \\\"quoted\\\" \\\\ $$USER %%i/lianli-daemon\" --service-invocation ${INVOCATION_ID}\n"));
        assert!(unit.contains("/lianli-control\" stop-service --invocation-id ${INVOCATION_ID}\n"));
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        assert_eq!(unit.matches("ExecStop=").count(), 2);
        assert!(unit.contains("KillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=120s"));
        let session = generate_session(
            "my box",
            Path::new("/usr/bin/distrobox-enter"),
            Path::new("/usr/bin"),
        )
        .unwrap();
        assert!(session.contains("KillMode=control-group\nSendSIGKILL=no\nTimeoutStopSec=30s"));
        assert!(session.contains("--stop-service ${INVOCATION_ID}\n"));
    }

    #[test]
    fn unit_refuses_directive_injection_and_ambiguous_executables() {
        for name in [
            "",
            "-option",
            "box\nExecStart=/bin/false",
            "box';exit",
            "box/other",
        ] {
            assert!(generate(
                name,
                Path::new("/usr/bin/distrobox-enter"),
                Path::new("/usr/bin")
            )
            .is_err());
            assert!(guidance(name).is_err());
        }
        for path in [
            "relative/distrobox-enter",
            "/usr/bin/sh",
            "/opt/$HOME/distrobox-enter",
            "/opt/line\nbreak/distrobox-enter",
        ] {
            assert!(generate("box", Path::new(path), Path::new("/usr/bin")).is_err());
        }
        assert!(generate(
            "box",
            Path::new("/usr/bin/distrobox-enter"),
            Path::new("relative")
        )
        .is_err());
    }
}
