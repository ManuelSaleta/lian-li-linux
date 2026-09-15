use crate::services::Route;
use anyhow::{ensure, Result};
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

pub(super) fn inspect(route: &Route) -> Result<Option<String>> {
    let read = |property| -> Result<Commands> {
        let output = route.output(
            "/usr/bin/busctl",
            &[
                "--user",
                "--timeout=4",
                "--json=short",
                "get-property",
                "org.freedesktop.systemd1",
                "/org/freedesktop/systemd1/unit/lianli_2ddaemon_2eservice",
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
    match_recipe(read("ExecStartEx")?, read("ExecStopEx")?)
}

fn match_recipe(start: Commands, stop: Commands) -> Result<Option<String>> {
    ensure!(
        start.signature == "a(sasasttttuii)" && stop.signature == start.signature,
        "Unrecognized systemd command property format"
    );
    let ([start], [stop]) = (start.data.as_slice(), stop.data.as_slice()) else {
        return Ok(None);
    };
    let (
        [start_program, start_name, box_name, separator, daemon, invocation_option, invocation],
        [stop_program, stop_name, stop_box, stop_separator, control, command, stop_option, stop_invocation],
    ) = (start.1.as_slice(), stop.1.as_slice())
    else {
        return Ok(None);
    };
    let box_valid = !box_name.is_empty()
        && box_name.len() <= 255
        && box_name.as_bytes()[0].is_ascii_alphanumeric()
        && box_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
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
        && start.0 == *start_program
        && stop.0 == *stop_program
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

    fn commands(args: &[&str]) -> Commands {
        serde_json::from_value(serde_json::json!({"type":"a(sasasttttuii)","data":[[
            args[0], args, [], 0,0,0,0,0,0,0
        ]]}))
        .unwrap()
    }

    #[test]
    fn only_matching_single_start_and_stop_recipes_are_accepted() {
        let start = [
            "/usr/bin/distrobox-enter",
            "--name",
            "box",
            "--",
            "/usr/bin/lianli-daemon",
            "--service-invocation",
            "${INVOCATION_ID}",
        ];
        let stop = [
            "/usr/bin/distrobox-enter",
            "--name",
            "box",
            "--",
            "/usr/bin/lianli-control",
            "stop-service",
            "--invocation-id",
            "${INVOCATION_ID}",
        ];
        assert_eq!(
            match_recipe(commands(&start), commands(&stop))
                .unwrap()
                .as_deref(),
            Some("box")
        );
        for (index, replacement) in [
            (0, "/bin/sh"),
            (2, "another-box"),
            (4, "/usr/bin/kill"),
            (5, "diagnose"),
            (7, "stale-id"),
        ] {
            let mut wrong = stop;
            wrong[index] = replacement;
            assert!(match_recipe(commands(&start), commands(&wrong))
                .unwrap()
                .is_none());
        }
        let mut joined = commands(&stop);
        joined.data[0].1 = vec![stop.join(" ")];
        assert!(match_recipe(commands(&start), joined).unwrap().is_none());
        let mut multiple = commands(&stop);
        multiple.data.push(multiple.data[0].clone());
        assert!(match_recipe(commands(&start), multiple).unwrap().is_none());
        let mut ignored = commands(&stop);
        ignored.data[0].2 = vec!["ignore-failure".into()];
        assert!(match_recipe(commands(&start), ignored).unwrap().is_none());
    }
}
