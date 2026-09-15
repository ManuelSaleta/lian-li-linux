use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationFinding, InstallationGuide,
};
use std::process::Command;
use std::time::Duration;

pub fn inspect() -> Vec<InstallationFinding> {
    let mut encoder = Command::new("ffmpeg");
    encoder.args(["-hide_banner", "-encoders"]);
    let mut probe = Command::new("ffprobe");
    probe.arg("-version");
    vec![
        evaluate(
            "ffmpeg",
            crate::command::run(encoder, Duration::from_secs(1)),
        ),
        evaluate(
            "ffprobe",
            crate::command::run(probe, Duration::from_secs(1)),
        ),
    ]
}

fn evaluate(tool: &str, output: anyhow::Result<crate::command::Output>) -> InstallationFinding {
    let (state, evidence) = match output {
        Ok(output) if output.status.success() => {
            if tool == "ffmpeg"
                && !output.stdout.lines().any(|line| {
                    let mut fields = line.split_whitespace();
                    fields.next().is_some_and(|flags| flags.starts_with('V'))
                        && fields.next() == Some("libx264")
                })
            {
                (
                    CheckState::Passed,
                    "ffmpeg is available but does not list the libx264 software encoder".into(),
                )
            } else {
                (
                    CheckState::Passed,
                    format!(
                        "{tool} runs in the daemon environment{}",
                        if tool == "ffmpeg" {
                            " and lists libx264"
                        } else {
                            ""
                        }
                    ),
                )
            }
        }
        Ok(output) => (
            CheckState::Failed,
            format!("{tool} exited with {}", output.status),
        ),
        Err(error) => (
            CheckState::Unavailable,
            format!("{tool} could not be checked: {error:#}"),
        ),
    };
    InstallationFinding {
        code: format!("media.tools.{tool}"), state,
        severity: if state == CheckState::Passed { FindingSeverity::Info } else { FindingSeverity::Warning },
        feature: "Configured LCD media".into(), context: "Selected daemon".into(),
        title: format!("{tool} availability"), evidence,
        remediation: "Install FFmpeg with ffprobe and libx264 in the daemon's environment (inside its Distrobox when applicable). Verify the service PATH, then Recheck and save the failed media settings to retry. No GPU encoder is required.".into(),
        guide: InstallationGuide::Troubleshooting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(stdout: &str) -> anyhow::Result<crate::command::Output> {
        Ok(crate::command::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    #[test]
    fn software_encoder_detection_requires_an_encoder_entry_not_a_description() {
        assert_eq!(
            evaluate("ffmpeg", output(" V....D libx264 H.264 encoder\n")).state,
            CheckState::Passed
        );
        assert!(
            evaluate("ffmpeg", output(" V....D libx264 H.264 encoder\n"))
                .evidence
                .contains("and lists libx264")
        );
        assert!(
            evaluate("ffmpeg", output(" V....D h264_nvenc libx264 replacement\n"))
                .evidence
                .contains("does not list")
        );
        assert!(
            evaluate("ffmpeg", output(" V....D libx264rgb RGB encoder\n"))
                .evidence
                .contains("does not list")
        );
        assert_eq!(
            evaluate("ffprobe", output("ffprobe version 7\n")).state,
            CheckState::Passed
        );
        assert_eq!(
            evaluate("ffprobe", Err(anyhow::anyhow!("not found"))).state,
            CheckState::Unavailable
        );
        let mut failed = output("").unwrap();
        failed.status = std::process::ExitStatus::from_raw(256);
        assert_eq!(evaluate("ffprobe", Ok(failed)).state, CheckState::Failed);
    }
}
