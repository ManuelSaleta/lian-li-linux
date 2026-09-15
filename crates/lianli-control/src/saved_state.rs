use crate::account::Account;
use crate::destination::Destination;
use crate::state::StateSnapshot;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checked {
    pub state: Option<String>,
    pub assets: String,
}

pub fn inspect(config: &Path, working: &Path, decode: bool) -> Result<Checked> {
    crate::container_destination::verify_config(config)?;
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Validate saved settings under their unprivileged account"
    );
    inspect_with(config, working, |dependencies| {
        if decode {
            crate::media_validation::ensure_decodable(dependencies)?;
        }
        Ok(())
    })
}

pub fn check(account: &Account, destination: &Destination, decode: bool) -> Result<Checked> {
    ensure!(
        account.matches_destination(destination),
        "Saved-state account differs from destination preflight"
    );
    let mut args = vec![
        OsStr::new("check-saved-state"),
        OsStr::new("--config"),
        destination.config_path.as_os_str(),
        OsStr::new("--working-directory"),
        destination.working_directory.as_os_str(),
    ];
    if decode {
        args.push(OsStr::new("--decode"));
    }
    let output = crate::command::run(
        account.control_command(&args)?,
        Duration::from_secs(if decode { 630 } else { 30 }),
    )?;
    ensure!(
        output.status.success(),
        "Saved-state validation failed: {}",
        output.stderr.trim()
    );
    let checked: Checked =
        serde_json::from_str(&output.stdout).context("Invalid saved-state validation response")?;
    ensure!(
        checked.state.as_deref()
            == destination
                .state
                .as_ref()
                .map(|state| state.generation.as_str()),
        "Saved settings changed after destination preflight. Prepare the switch again"
    );
    Ok(checked)
}

fn inspect_with(
    config: &Path,
    working: &Path,
    validate: impl FnOnce(&[lianli_shared::media_dependencies::AssetDependency]) -> Result<()>,
) -> Result<Checked> {
    ensure!(
        config.is_absolute() && working.is_absolute(),
        "Saved-state paths must be absolute"
    );
    let parent = config.parent().context("Saved state has no directory")?;
    let held = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = held.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "Saved-state directory has unexpected ownership or permissions"
    );
    let snapshot = match fs::symlink_metadata(config) {
        Ok(_) => Some(StateSnapshot::read(config, working)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_empty(config)?;
            None
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(snapshot) = &snapshot {
        ensure!(
            snapshot.summary.issue_count == 0,
            "Saved settings contain invalid references"
        );
    }
    let dependencies = snapshot
        .as_ref()
        .map_or(&[][..], |snapshot| snapshot.dependencies.as_slice());
    let before = asset_fingerprint(dependencies)?;
    validate(dependencies)?;
    ensure!(
        asset_fingerprint(dependencies)? == before,
        "Saved media changed during validation"
    );
    if let Some(snapshot) = &snapshot {
        snapshot.verify_unchanged(config, working)?;
    } else {
        ensure_empty(config)?;
    }
    let current = fs::symlink_metadata(parent)?;
    ensure!(
        current.is_dir() && (current.dev(), current.ino()) == (metadata.dev(), metadata.ino()),
        "Saved-state directory changed during validation"
    );
    Ok(Checked {
        state: snapshot.map(|snapshot| snapshot.summary.generation),
        assets: before,
    })
}

fn ensure_empty(config: &Path) -> Result<()> {
    let parent = config.parent().context("Saved state has no directory")?;
    for path in [
        config.to_path_buf(),
        parent.join("lcd_templates.json"),
        parent.join("rgb_presets.json"),
        parent.join("profiles"),
    ] {
        ensure!(
            fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "Destination settings appeared or auxiliary state exists without its configuration"
        );
    }
    Ok(())
}

fn asset_fingerprint(
    dependencies: &[lianli_shared::media_dependencies::AssetDependency],
) -> Result<String> {
    ensure!(
        dependencies.len() <= 4096,
        "Too many saved media references"
    );
    let mut hash = Sha256::new();
    for dependency in dependencies {
        let path = dependency.path.as_os_str().as_encoded_bytes();
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path);
        let file = match crate::state::open_media_source(&dependency.path) {
            Ok(file) => {
                hash.update(b"readable");
                file
            }
            Err(error) => {
                // Unused destination state is backed up, not played or copied. Keep its failure state comparable.
                let error = format!("{error:#}");
                hash.update(b"unavailable");
                hash.update((error.len() as u64).to_le_bytes());
                hash.update(error.as_bytes());
                continue;
            }
        };
        let metadata = file.metadata()?;
        for value in [
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mode() as u64,
            metadata.uid() as u64,
            metadata.gid() as u64,
        ] {
            hash.update(value.to_le_bytes());
        }
        for value in [
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ] {
            hash.update(value.to_le_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_settings_changes_and_accepts_unchanged_state() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let before = inspect_with(&config, root.path(), |_| Ok(())).unwrap();
        assert!(before.state.is_some());
        assert_eq!(
            inspect_with(&config, root.path(), |_| Ok(())).unwrap(),
            before
        );
        assert!(inspect_with(&config, root.path(), |_| {
            fs::write(&config, b"{\"hardware_video\":true}")?;
            Ok(())
        })
        .is_err());
    }

    #[test]
    fn saved_media_and_unused_template_children_are_revalidated_after_decoding() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(
            &config,
            br#"{"lcds":[{"index":0,"type":"image","path":"active.png"}]}"#,
        )
        .unwrap();
        fs::write(root.path().join("active.png"), b"fixture pixels").unwrap();
        fs::write(root.path().join("child.png"), b"template pixels").unwrap();
        fs::write(root.path().join("lcd_templates.json"), br#"{"templates":[{"id":"unused","name":"Unused","base_width":400,"base_height":400,"background":{"type":"image","path":"child.png"},"widgets":[]}]}"#).unwrap();
        let before = inspect_with(&config, root.path(), |dependencies| {
            assert!(dependencies
                .iter()
                .any(|dependency| dependency.path.ends_with("active.png")));
            assert!(dependencies
                .iter()
                .any(|dependency| dependency.path.ends_with("child.png")));
            Ok(())
        })
        .unwrap();
        assert!(inspect_with(&config, root.path(), |_| {
            fs::write(root.path().join("child.png"), b"changed template pixels")?;
            Ok(())
        })
        .is_err());
        let changed = inspect_with(&config, root.path(), |_| Ok(())).unwrap();
        assert_eq!(before.state, changed.state);
        assert_ne!(before.assets, changed.assets);
        fs::remove_file(root.path().join("active.png")).unwrap();
        let missing = inspect_with(&config, root.path(), |_| Ok(())).unwrap();
        assert_ne!(missing.assets, changed.assets);
        assert_eq!(
            missing,
            inspect_with(&config, root.path(), |_| Ok(())).unwrap()
        );
        assert!(inspect_with(&config, root.path(), |_| anyhow::bail!(
            "selected media is unavailable"
        ))
        .is_err());
    }

    #[test]
    fn missing_state_must_remain_empty_and_have_no_orphaned_auxiliary_files() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        assert!(inspect_with(&config, root.path(), |_| Ok(()))
            .unwrap()
            .state
            .is_none());
        assert!(inspect_with(&config, root.path(), |_| {
            fs::write(&config, b"{}").unwrap();
            Ok(())
        })
        .is_err());
        fs::remove_file(&config).unwrap();
        fs::write(root.path().join("lcd_templates.json"), b"{}").unwrap();
        assert!(inspect_with(&config, root.path(), |_| Ok(())).is_err());
    }
}
