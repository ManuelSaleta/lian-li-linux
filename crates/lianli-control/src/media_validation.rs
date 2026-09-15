use crate::media_staging::SourceIdentity;
use anyhow::{ensure, Context, Result};
use lianli_shared::media_dependencies::{
    AssetAccessIssue, AssetAccessReport, AssetDependency, AssetKind,
};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Decoded {
    identity: SourceIdentity,
    result: Result<(), String>,
}

pub fn ensure_decodable(dependencies: &[AssetDependency]) -> Result<()> {
    ensure_decodable_checked(dependencies, || Ok(()))
}

pub(crate) fn ensure_decodable_checked(
    dependencies: &[AssetDependency],
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    check()?;
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Validate media under its unprivileged destination account"
    );
    if dependencies.is_empty() {
        return Ok(());
    }
    crate::destination::verify_daemon_binary()?;
    let report = check_with(dependencies, |file, path, kind, timeout| {
        check()?;
        let mut command = Command::new("/usr/bin/lianli-daemon");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C.UTF-8")
            .env("container", "lianli-media-validation")
            .args(["check-media-decode", "--kind", kind]);
        if kind == "image" {
            if let Some(extension) = path.extension() {
                command.arg("--extension").arg(extension);
            }
        }
        let output =
            crate::command::run_with_stdin(command, Stdio::from(file.try_clone()?), timeout)?;
        check()?;
        ensure!(
            output.status.success(),
            "Decoder failed ({}): {}",
            output.status,
            output.stderr.chars().take(512).collect::<String>()
        );
        ensure!(
            output.stdout.is_empty(),
            "Decoder returned unexpected output"
        );
        Ok(())
    })?;
    check()?;
    if let Some(issue) = report.issues.first() {
        anyhow::bail!(
            "{} media references failed destination validation: {}: {} ({})",
            report.failed,
            issue.owner,
            issue.path.as_deref().unwrap_or(Path::new("")).display(),
            issue.error
        );
    }
    Ok(())
}

fn check_with(
    dependencies: &[AssetDependency],
    mut decode: impl FnMut(&File, &Path, &'static str, Duration) -> Result<()>,
) -> Result<AssetAccessReport> {
    ensure!(
        dependencies.len() <= 4096,
        "Media validation supports at most 4096 references"
    );
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut cache: HashMap<(PathBuf, &'static str), Decoded> = HashMap::new();
    let mut report = AssetAccessReport {
        uid: unsafe { libc::geteuid() },
        checked: 0,
        failed: 0,
        issues: Vec::new(),
    };
    for dependency in dependencies {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "Media validation exceeded ten minutes"
        );
        let kind = match dependency.kind {
            AssetKind::File => "file",
            AssetKind::Image => "image",
            AssetKind::Video => "video",
            AssetKind::Gif => "gif",
            AssetKind::Font => "font",
        };
        let checked = (|| {
            let file = crate::state::open_media_source(&dependency.path)
                .context("Opening destination media")?;
            let before = SourceIdentity::from(&file.metadata()?);
            let key = (dependency.path.clone(), kind);
            let result = match cache.get(&key) {
                Some(decoded) if decoded.identity == before => decoded.result.clone(),
                _ => {
                    let result = decode(
                        &file,
                        &dependency.path,
                        kind,
                        remaining.min(Duration::from_secs(25)),
                    )
                    .map_err(|error| format!("{error:#}").chars().take(512).collect::<String>());
                    cache.insert(
                        key,
                        Decoded {
                            identity: before,
                            result: result.clone(),
                        },
                    );
                    result
                }
            };
            ensure!(
                SourceIdentity::from(&file.metadata()?) == before
                    && SourceIdentity::from(
                        &crate::state::open_media_source(&dependency.path)?.metadata()?
                    ) == before,
                "Destination media changed during decoding"
            );
            result.map_err(anyhow::Error::msg)
        })();
        report.checked += 1;
        if let Err(error) = checked {
            report.failed += 1;
            if report.issues.len() < 32 {
                report.issues.push(AssetAccessIssue {
                    owner: dependency.owner.clone(),
                    path: Some(dependency.path.clone()),
                    error: format!("{error:#}").chars().take(512).collect(),
                });
            }
        }
    }
    for ((path, _), decoded) in cache {
        ensure!(
            Instant::now() < deadline,
            "Media validation exceeded ten minutes"
        );
        ensure!(
            SourceIdentity::from(&crate::state::open_media_source(&path)?.metadata()?)
                == decoded.identity,
            "Destination media changed after decoding: {}",
            path.display()
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn caches_each_decoder_kind_and_reports_every_affected_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, b"pixels").unwrap();
        let dependencies = [AssetKind::Image, AssetKind::Image, AssetKind::Font]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| AssetDependency {
                path: path.clone(),
                owner: format!("owner {index}"),
                kind,
            })
            .collect::<Vec<_>>();
        let mut calls = 0;
        let report = check_with(&dependencies, |_, _, kind, _| {
            calls += 1;
            ensure!(kind == "font", "invalid image");
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            (report.checked, report.failed, report.issues.len()),
            (3, 2, 2)
        );
        assert_eq!(report.issues[1].owner, "owner 1");
    }

    #[test]
    fn changed_and_replaced_files_are_rejected_and_never_reuse_old_decode_results() {
        for replace in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("file");
            fs::write(&path, b"original").unwrap();
            let dependency = AssetDependency {
                path: path.clone(),
                owner: "template child".into(),
                kind: AssetKind::Video,
            };
            let mut calls = 0;
            let report = check_with(&[dependency.clone(), dependency], |_, _, _, _| {
                calls += 1;
                if calls == 1 {
                    if replace {
                        fs::remove_file(&path).unwrap();
                    }
                    fs::write(&path, b"new content").unwrap();
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(calls, 2);
            assert_eq!(report.failed, 1);
            assert!(report.issues[0].error.contains("changed during decoding"));
        }
    }
}
