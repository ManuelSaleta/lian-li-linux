use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    UserDaemon,
    SystemDaemon,
    SessionWorker,
    SavedDaemonLog,
}

pub fn read(context: &InstallationContext, source: LogSource) -> Result<Vec<String>> {
    let (scope, unit) = match source {
        LogSource::UserDaemon => ("--user", "lianli-daemon.service"),
        LogSource::SystemDaemon => ("--system", "lianli-daemon-system.service"),
        LogSource::SessionWorker => ("--user", "lianli-session.service"),
        LogSource::SavedDaemonLog => {
            anyhow::bail!("Choose a saved daemon log file in the export dialog")
        }
    };
    let route = crate::services::Route::detect(context)?;
    let state = route.output(
        "/usr/bin/systemctl",
        &[scope, "show", "--property=InvocationID", "--value", unit],
    )?;
    ensure!(
        state.status.success(),
        "Cannot inspect daemon invocation: {}",
        state.stderr.trim()
    );
    let invocation = if state.stdout.trim().is_empty() {
        let field = if scope == "--user" {
            "_SYSTEMD_USER_UNIT"
        } else {
            "_SYSTEMD_UNIT"
        };
        let recent = route.output(
            "/usr/bin/journalctl",
            &[
                scope,
                "--no-pager",
                "--lines=1",
                "--output=json",
                "--output-fields=_SYSTEMD_INVOCATION_ID",
                &format!("{field}={unit}"),
            ],
        )?;
        ensure!(
            recent.status.success(),
            "Cannot inspect latest daemon logs: {}",
            recent.stderr.trim()
        );
        latest_invocation(&recent.stdout)?
    } else {
        valid_invocation(state.stdout.trim())?.to_owned()
    };
    let output = route.output_with_limit(
        "/usr/bin/journalctl",
        &[
            scope,
            "--no-pager",
            "--lines=100",
            "--output=json",
            "--output-fields=MESSAGE",
            "--unit",
            unit,
            &format!("_SYSTEMD_INVOCATION_ID={invocation}"),
        ],
        1024 * 1024,
    )?;
    ensure!(
        output.status.success(),
        "Journal query unavailable: {}",
        output.stderr.trim()
    );
    messages(&output.stdout)
}

fn valid_invocation(value: &str) -> Result<&str> {
    ensure!(
        value.len() == 32
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            && value.bytes().any(|byte| byte != b'0'),
        "The service has no valid daemon invocation ID"
    );
    Ok(value)
}

fn latest_invocation(output: &str) -> Result<String> {
    let record: serde_json::Value = serde_json::from_str(output.trim())
        .context("No previous daemon invocation was found in the readable journal")?;
    Ok(valid_invocation(
        record
            .get("_SYSTEMD_INVOCATION_ID")
            .and_then(serde_json::Value::as_str)
            .context("Latest daemon log has no invocation ID")?,
    )?
    .to_owned())
}

pub fn plain_text(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut text = String::new();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else if !c.is_control() || c == '\n' || c == '\t' {
            text.push(c);
        }
    }
    text
}

fn message(record: &serde_json::Value) -> String {
    let text = match record.get("MESSAGE") {
        Some(serde_json::Value::String(value)) => Some(value.clone()),
        Some(serde_json::Value::Array(bytes)) => bytes
            .iter()
            .map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
            .collect::<Option<Vec<_>>>()
            .and_then(|bytes| String::from_utf8(bytes).ok()),
        _ => None,
    };
    text.map(|value| plain_text(&value))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "[non-text journal message omitted]".into())
}

fn messages(output: &str) -> Result<Vec<String>> {
    output
        .lines()
        .take(100)
        .map(|line| {
            let record: serde_json::Value = serde_json::from_str(line)?;
            Ok(message(&record))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colored_journal_byte_arrays_are_readable_and_invalid_bytes_are_rejected() {
        let colored = "\u{1b}[2m 6.9s\u{1b}[0m \u{1b}[32mINFO\u{1b}[0m Daemon initialized";
        let row = serde_json::json!({"MESSAGE": colored.as_bytes()});
        assert_eq!(message(&row), " 6.9s INFO Daemon initialized");
        assert_eq!(
            message(&serde_json::json!({"MESSAGE": colored})),
            " 6.9s INFO Daemon initialized"
        );
        for bytes in [
            serde_json::json!([256]),
            serde_json::json!([-1]),
            serde_json::json!([255]),
            serde_json::json!(["text"]),
        ] {
            assert_eq!(
                message(&serde_json::json!({"MESSAGE":bytes})),
                "[non-text journal message omitted]"
            );
        }
    }

    #[test]
    fn latest_daemon_invocation_requires_a_complete_nonzero_id() {
        let id = "00112233445566778899aabbccddeeff";
        assert_eq!(
            latest_invocation(&serde_json::json!({"_SYSTEMD_INVOCATION_ID":id}).to_string())
                .unwrap(),
            id
        );
        for value in [
            "",
            "--boot",
            "00000000000000000000000000000000",
            "x",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(valid_invocation(value).is_err());
        }
        assert!(latest_invocation("{}").is_err());
        assert!(latest_invocation("").is_err());
    }

    #[test]
    fn journal_export_selects_only_message_text_and_rejects_malformed_records() {
        let raw = "{\"MESSAGE\":\"Hermes-KMS selected\",\"_HOSTNAME\":\"private-host\",\"_CMDLINE\":\"private arguments\"}\n{\"MESSAGE\":[1,2]}";
        assert_eq!(
            messages(raw).unwrap(),
            vec!["Hermes-KMS selected", "[non-text journal message omitted]"]
        );
        assert!(messages("invalid JSON").is_err());
        let long = "a".repeat(4096);
        let text = serde_json::json!({"MESSAGE":long}).to_string();
        assert_eq!(messages(&text).unwrap()[0].len(), 4096);
        let rows = std::iter::repeat_n("{\"MESSAGE\":\"hello\"}", 101)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(messages(&rows).unwrap().len(), 100);
    }
}
