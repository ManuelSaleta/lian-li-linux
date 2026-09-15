use crate::services::Route;
use anyhow::{ensure, Result};
use lianli_shared::services::ServiceScope;
use serde::Deserialize;
use std::path::Path;

type Command = (
    String,
    Vec<String>,
    Vec<String>,
    u64,
    u64,
    u64,
    u64,
    u32,
    i32,
    i32,
);

#[derive(Deserialize)]
struct Commands {
    #[serde(rename = "type")]
    signature: String,
    data: Vec<Command>,
}

pub(super) fn inspect(route: &Route, scope: ServiceScope) -> Result<Option<String>> {
    let object = format!(
        "/org/freedesktop/systemd1/unit/{}",
        scope.unit().replace('-', "_2d").replace('.', "_2e")
    );
    let read = |property| -> Result<Commands> {
        let output = route.output(
            "/usr/bin/busctl",
            &[
                scope.argument(),
                "--timeout=4",
                "--json=short",
                "get-property",
                "org.freedesktop.systemd1",
                &object,
                "org.freedesktop.systemd1.Service",
                property,
            ],
        )?;
        ensure!(
            output.status.success(),
            "Cannot inspect Distrobox service commands with host busctl: {}",
            output.stderr.trim()
        );
        Ok(serde_json::from_str(&output.stdout)?)
    };
    match_scoped_recipe(scope, read("ExecStartEx")?, read("ExecStopEx")?)
}

fn match_scoped_recipe(
    scope: ServiceScope,
    mut start: Commands,
    stop: Commands,
) -> Result<Option<String>> {
    let [command] = start.data.as_mut_slice() else {
        return Ok(None);
    };
    if command
        .1
        .get(6)
        .is_some_and(|value| value == "/usr/bin/env")
    {
        let Some(working) = command
            .1
            .get(7)
            .and_then(|value| value.strip_prefix("--chdir="))
        else {
            return Ok(None);
        };
        if crate::container_destination::absolute(Path::new(working)).is_err() {
            return Ok(None);
        }
        command.1.drain(6..8);
        if scope == ServiceScope::User {
            let Some([option, config]) = command.1.get(7..9) else {
                return Ok(None);
            };
            if option != "--config"
                || crate::container_destination::validate_config(Path::new(config)).is_err()
            {
                return Ok(None);
            }
            command.1.drain(7..9);
        }
    }
    match scope {
        ServiceScope::User => match_recipe(start, stop),
        ServiceScope::System => match_system_recipe(start, stop),
    }
}

pub(super) fn system_owner_uid(route: &Route) -> Result<u32> {
    let output = route.output(
        "/usr/bin/systemctl",
        &[
            "--system",
            "--no-pager",
            "--no-ask-password",
            "show",
            "--property=User",
            "--value",
            ServiceScope::System.unit(),
        ],
    )?;
    ensure!(
        output.status.success(),
        "Cannot inspect the Distrobox system unit account: {}",
        output.stderr.trim()
    );
    parse_system_owner(&output.stdout)
}

fn parse_system_owner(value: &str) -> Result<u32> {
    let uid = value.trim().parse::<u32>().map_err(|_| {
        anyhow::anyhow!("The Distrobox system unit must specify its owner's numeric User ID")
    })?;
    ensure!(
        uid != 0,
        "The Distrobox system unit must run as its unprivileged owner"
    );
    Ok(uid)
}

fn match_system_recipe(mut start: Commands, mut stop: Commands) -> Result<Option<String>> {
    let ([started], [stopped, _]) = (start.data.as_mut_slice(), stop.data.as_mut_slice()) else {
        return Ok(None);
    };
    let Some([mode, option, config]) = started.1.get(7..10) else {
        return Ok(None);
    };
    if mode != "--system"
        || option != "--config"
        || !Path::new(config).is_absolute()
        || stopped.1.get(8..10) != Some(&["--scope".into(), "system".into()])
    {
        return Ok(None);
    }
    started.1.drain(7..10);
    stopped.1.drain(8..10);
    match_recipe(start, stop)
}

fn match_recipe(start: Commands, stop: Commands) -> Result<Option<String>> {
    ensure!(
        start.signature == "a(sasasttttuii)" && stop.signature == start.signature,
        "Unrecognized systemd command property format"
    );
    let ([start], [stop, wait]) = (start.data.as_slice(), stop.data.as_slice()) else {
        return Ok(None);
    };
    if wait.0 != "/usr/bin/sh"
        || !wait.2.is_empty()
        || wait.1
            != [
                "/usr/bin/sh",
                "-c",
                &crate::distrobox_unit::WRAPPER_WAIT_SCRIPT.replace('$', "$$"),
                "--",
                "${MAINPID}",
            ]
    {
        return Ok(None);
    }
    let (
        [start_env, start_unset, start_program, start_name, box_name, separator, daemon, invocation_option, invocation],
        [stop_env, stop_unset, stop_program, stop_name, stop_box, stop_separator, control, command, stop_option, stop_invocation],
    ) = (start.1.as_slice(), stop.1.as_slice())
    else {
        return Ok(None);
    };
    let box_valid = crate::distrobox_unit::valid_name(box_name);
    let program = Path::new(start_program);
    let daemon = Path::new(daemon);
    let control = Path::new(control);
    let matches = box_valid
        && start.2.is_empty()
        && stop.2.is_empty()
        && program.is_absolute()
        && program
            .file_name()
            .is_some_and(|name| name == "distrobox-enter")
        && start.0 == "/usr/bin/env"
        && stop.0 == start.0
        && start_env == &start.0
        && stop_env == start_env
        && start_unset == "--unset=INVOCATION_ID"
        && stop_unset == start_unset
        && start_program == stop_program
        && start_name == "--name"
        && stop_name == start_name
        && box_name == stop_box
        && separator == "--"
        && stop_separator == separator
        && daemon.is_absolute()
        && control.is_absolute()
        && daemon.parent() == control.parent()
        && daemon
            .file_name()
            .is_some_and(|name| name == "lianli-daemon")
        && control
            .file_name()
            .is_some_and(|name| name == "lianli-control")
        && invocation_option == "--service-invocation"
        && invocation == "${INVOCATION_ID}"
        && command == "stop-service"
        && stop_option == "--invocation-id"
        && stop_invocation == invocation;
    Ok(matches.then(|| box_name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_wrapper_owner_requires_one_non_root_numeric_uid() {
        assert_eq!(parse_system_owner("1000\n").unwrap(), 1000);
        for value in ["", "0", "root", "lianli", "1000\n2000", "-1", "4294967296"] {
            assert!(parse_system_owner(value).is_err());
        }
    }

    fn commands(args: &[&str]) -> Commands {
        serde_json::from_value(serde_json::json!({"type":"a(sasasttttuii)","data":[[
            args[0], args, [], 0,0,0,0,0,0,0
        ]]}))
        .unwrap()
    }

    #[test]
    fn only_matching_guarded_stop_and_bounded_wrapper_wait_are_accepted() {
        let start = [
            "/usr/bin/env",
            "--unset=INVOCATION_ID",
            "/usr/bin/distrobox-enter",
            "--name",
            "box",
            "--",
            "/usr/bin/lianli-daemon",
            "--service-invocation",
            "${INVOCATION_ID}",
        ];
        let stop = [
            "/usr/bin/env",
            "--unset=INVOCATION_ID",
            "/usr/bin/distrobox-enter",
            "--name",
            "box",
            "--",
            "/usr/bin/lianli-control",
            "stop-service",
            "--invocation-id",
            "${INVOCATION_ID}",
        ];
        let stopped = |args: &[&str]| {
            let mut result = commands(args);
            result.data.extend(
                commands(&[
                    "/usr/bin/sh",
                    "-c",
                    &crate::distrobox_unit::WRAPPER_WAIT_SCRIPT.replace('$', "$$"),
                    "--",
                    "${MAINPID}",
                ])
                .data,
            );
            result
        };
        assert_eq!(
            match_recipe(commands(&start), stopped(&stop))
                .unwrap()
                .as_deref(),
            Some("box")
        );
        let system_start = || {
            let mut value = commands(&start);
            value.data[0].1.splice(
                7..7,
                ["--system", "--config", "/state/config.json"].map(str::to_owned),
            );
            value
        };
        for scope in [ServiceScope::User, ServiceScope::System] {
            let mut managed = commands(&start);
            managed.data[0].1.splice(
                6..6,
                ["/usr/bin/env", "--chdir=/home/box owner"].map(str::to_owned),
            );
            let options = if scope == ServiceScope::System {
                vec!["--system", "--config", "/state/config.json"]
            } else {
                vec!["--config", "/state/config.json"]
            };
            managed.data[0]
                .1
                .splice(9..9, options.into_iter().map(str::to_owned));
            let mut stopping = stopped(&stop);
            if scope == ServiceScope::System {
                stopping.data[0]
                    .1
                    .splice(8..8, ["--scope", "system"].map(str::to_owned));
            }
            assert_eq!(
                match_scoped_recipe(scope, managed, stopping)
                    .unwrap()
                    .as_deref(),
                Some("box")
            );
        }
        let system_stop = || {
            let mut value = stopped(&stop);
            value.data[0]
                .1
                .splice(8..8, ["--scope", "system"].map(str::to_owned));
            value
        };
        assert_eq!(
            match_system_recipe(system_start(), system_stop())
                .unwrap()
                .as_deref(),
            Some("box")
        );
        assert!(match_recipe(system_start(), system_stop())
            .unwrap()
            .is_none());
        assert!(match_system_recipe(commands(&start), stopped(&stop))
            .unwrap()
            .is_none());
        let mut wrong_config = system_start();
        wrong_config.data[0].1[9] = "relative.json".into();
        assert!(match_system_recipe(wrong_config, system_stop())
            .unwrap()
            .is_none());
        let mut wrong_scope = system_stop();
        wrong_scope.data[0].1[9] = "user".into();
        assert!(match_system_recipe(system_start(), wrong_scope)
            .unwrap()
            .is_none());
        for (index, replacement) in [
            (0, "/bin/sh"),
            (1, "--unset=OTHER"),
            (4, "another-box"),
            (6, "/usr/bin/kill"),
            (7, "diagnose"),
            (9, "stale-id"),
        ] {
            let mut wrong = stop;
            wrong[index] = replacement;
            assert!(match_recipe(commands(&start), stopped(&wrong))
                .unwrap()
                .is_none());
        }
        let mut joined = stopped(&stop);
        joined.data[0].1 = vec![stop.join(" ")];
        assert!(match_recipe(commands(&start), joined).unwrap().is_none());
        let mut multiple = stopped(&stop);
        multiple.data.push(multiple.data[0].clone());
        assert!(match_recipe(commands(&start), multiple).unwrap().is_none());
        let mut ignored = stopped(&stop);
        ignored.data[0].2 = vec!["ignore-failure".into()];
        assert!(match_recipe(commands(&start), ignored).unwrap().is_none());
        let mut spaced_start = start;
        let mut spaced_stop = stop;
        spaced_start[2] = "/opt/host tools/distrobox-enter";
        spaced_stop[2] = spaced_start[2];
        spaced_start[4] = "my box";
        spaced_stop[4] = spaced_start[4];
        spaced_start[6] = "/opt/app tools/lianli-daemon";
        spaced_stop[6] = "/opt/app tools/lianli-control";
        assert_eq!(
            match_recipe(commands(&spaced_start), stopped(&spaced_stop))
                .unwrap()
                .as_deref(),
            Some("my box")
        );
        assert!(match_recipe(commands(&start[2..]), commands(&stop[2..]))
            .unwrap()
            .is_none());
        assert!(match_recipe(commands(&start), commands(&stop))
            .unwrap()
            .is_none());
        for (index, replacement) in [(0, "/bin/sh"), (2, "sleep 999"), (4, "123")] {
            let mut altered = stopped(&stop);
            altered.data[1].1[index] = replacement.into();
            assert!(match_recipe(commands(&start), altered).unwrap().is_none());
        }
    }
}
