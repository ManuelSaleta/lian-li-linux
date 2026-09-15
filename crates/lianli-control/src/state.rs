use anyhow::{ensure, Context, Result};
use lianli_shared::config::AppConfig;
use lianli_shared::media_dependencies::{
    lcd_dependencies, stored_lcd_dependencies, template_dependencies, validate_dependency_input,
    validate_dependency_paths, AssetAccessIssue, AssetAccessReport, AssetDependency,
};
use lianli_shared::profile::DeviceProfile;
use lianli_shared::rgb::RgbPreset;
use lianli_shared::template::LcdTemplate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROFILES: usize = 256;
const MAX_DEPENDENCIES: usize = 4096;
const MAX_ISSUES: usize = 32;

pub struct StateFile {
    pub relative_path: PathBuf,
    pub bytes: Vec<u8>,
}

pub struct StateSnapshot {
    pub files: Vec<StateFile>,
    pub dependencies: Vec<AssetDependency>,
    pub summary: StateSummary,
    config_directory: PathBuf,
    working_directory: PathBuf,
    source_uid: u32,
    source_namespace: (u64, u64),
    source_config_path: PathBuf,
}

pub struct PreparedState {
    pub media: crate::media_staging::MediaStaging,
    pub files: Vec<StateFile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StateSummary {
    pub generation: String,
    pub files: usize,
    pub bytes: usize,
    pub profiles: usize,
    pub templates: usize,
    pub asset_references: usize,
    pub issue_count: usize,
    pub issues: Vec<String>,
}

#[derive(Default, Deserialize)]
struct TemplateFile {
    #[serde(default)]
    templates: Vec<LcdTemplate>,
}

impl StateSnapshot {
    /// Run under the source account and mount namespace. This inventories references;
    /// destination readability and generation checks before and after clean shutdown are
    /// required before replacing state.
    pub fn read(config_path: &Path, working_directory: &Path) -> Result<Self> {
        let source_uid = unsafe { libc::geteuid() };
        let source_namespace = mount_namespace()?;
        ensure!(
            working_directory.is_absolute(),
            "The daemon working directory must be absolute"
        );
        let config_path = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            working_directory.join(config_path)
        };
        let base = config_path
            .parent()
            .context("Configuration has no parent directory")?;
        let name = config_path
            .file_name()
            .context("Configuration has no filename")?;
        ensure!(
            name != "lcd_templates.json" && name != "rgb_presets.json",
            "Configuration filename conflicts with auxiliary state"
        );
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(base)
            .with_context(|| format!("Opening state directory {}", base.display()))?;
        let mut generation = Sha256::new();
        generation.update(b"lianli-state-v1\0");
        for path in [&config_path, working_directory] {
            let bytes = path.as_os_str().as_bytes();
            generation.update((bytes.len() as u64).to_le_bytes());
            generation.update(bytes);
        }
        hash_directory(&mut generation, &directory)?;
        let mut files = Vec::new();
        let mut total = 0;
        let config_file = read_file(&directory, name, PathBuf::from(name), &mut total)?
            .with_context(|| format!("Configuration {} is missing", config_path.display()))?;
        let config: AppConfig = parse(&config_file)?;
        ensure!(
            config.default_fps.is_finite() && config.default_fps > 0.0,
            "default_fps must be greater than zero"
        );
        files.push(config_file);
        let templates = match read_file(
            &directory,
            OsStr::new("lcd_templates.json"),
            "lcd_templates.json".into(),
            &mut total,
        )? {
            Some(file) => {
                let parsed: TemplateFile = parse(&file)?;
                files.push(file);
                parsed.templates
            }
            None => Vec::new(),
        };
        if let Some(file) = read_file(
            &directory,
            OsStr::new("rgb_presets.json"),
            "rgb_presets.json".into(),
            &mut total,
        )? {
            let _: Vec<RgbPreset> = parse(&file)?;
            files.push(file);
        }
        let mut profiles = Vec::new();
        if let Some(directory) = open_entry(&directory, OsStr::new("profiles"), libc::O_DIRECTORY)?
        {
            hash_directory(&mut generation, &directory)?;
            let mut names = Vec::new();
            let entries = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
                .context("Listing saved profiles")?;
            for (index, entry) in entries.enumerate() {
                ensure!(index < 4096, "Profile directory has more than 4096 entries");
                let name = entry
                    .context("Reading profile directory entry")?
                    .file_name();
                if Path::new(&name).extension() == Some(OsStr::new("json")) {
                    ensure!(
                        names.len() < MAX_PROFILES,
                        "More than 256 saved profiles. Migration requires a smaller state set"
                    );
                    ensure!(
                        name.to_str().is_some(),
                        "Profile filename is not valid UTF-8"
                    );
                    names.push(name);
                }
            }
            names.sort();
            for name in names {
                let file = read_file(
                    &directory,
                    &name,
                    Path::new("profiles").join(&name),
                    &mut total,
                )?
                .context("A saved profile disappeared during inspection. Retry")?;
                let profile: DeviceProfile = parse(&file)?;
                ensure!(
                    profile.schema_version == 1,
                    "{} uses unsupported profile schema {}",
                    file.relative_path.display(),
                    profile.schema_version
                );
                profiles.push((file.relative_path.clone(), profile));
                files.push(file);
            }
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        for file in &files {
            let name = file.relative_path.as_os_str().as_bytes();
            generation.update((name.len() as u64).to_le_bytes());
            generation.update(name);
            generation.update((file.bytes.len() as u64).to_le_bytes());
            generation.update(&file.bytes);
        }
        let mut summary = StateSummary {
            generation: format!("{:x}", generation.finalize()),
            files: files.len(),
            bytes: total,
            profiles: profiles.len(),
            templates: templates.len(),
            asset_references: 0,
            issue_count: 0,
            issues: Vec::new(),
        };
        validate_dependency_input(&config.lcds, &templates).map_err(anyhow::Error::msg)?;
        for (_, profile) in &profiles {
            validate_dependency_input(&profile.lcds, &[]).map_err(anyhow::Error::msg)?;
        }
        let mut dependencies = Vec::new();
        let mut template_ids = std::collections::HashSet::new();
        for template in &templates {
            if !template_ids.insert(&template.id) {
                issue(
                    &mut summary,
                    format!("Duplicate template ID '{}'", template.id),
                );
            }
            if let Err(error) = template.validate() {
                issue(&mut summary, error);
            }
            append_dependencies(
                &mut dependencies,
                template_dependencies(template),
                "Saved templates",
                working_directory,
            )?;
        }
        for (owner, lcds) in std::iter::once(("Configuration".to_string(), config.lcds)).chain(
            profiles
                .into_iter()
                .map(|(path, profile)| (format!("Profile '{}'", path.display()), profile.lcds)),
        ) {
            for mut lcd in lcds {
                lcd.resolve_paths(base);
                if let Err(error) = lcd.validate_settings() {
                    issue(&mut summary, format!("{owner}: {error}"));
                }
                if let Err(error) = lcd_dependencies(&lcd, &templates) {
                    issue(&mut summary, format!("{owner}: {error}"));
                }
                append_dependencies(
                    &mut dependencies,
                    stored_lcd_dependencies(&lcd),
                    &owner,
                    working_directory,
                )?;
            }
        }
        summary.asset_references = dependencies.len();
        Ok(Self {
            files,
            dependencies,
            summary,
            config_directory: base.to_path_buf(),
            working_directory: working_directory.to_path_buf(),
            source_uid,
            source_namespace,
            source_config_path: config_path.clone(),
        })
    }

    /// Copies sources under the snapshot's account and namespace. A privileged
    /// destination installer must receive readable descriptors from this account.
    pub fn prepare_media(
        &self,
        staging_parent: &Path,
        destination: &Path,
        control: &crate::media_staging::CopyControl,
    ) -> Result<PreparedState> {
        control.check()?;
        ensure!(
            self.source_uid == unsafe { libc::geteuid() }
                && self.source_namespace == mount_namespace()?,
            "Media sources must be opened under the snapshot's account and mount namespace"
        );
        ensure!(
            self.summary.issue_count == 0,
            "Repair the state inventory's structural issues before preparing migration"
        );
        let access = self.check_assets(control)?;
        if let Some(issue) = access.issues.first() {
            anyhow::bail!("{} saved asset references are inaccessible. {} cannot read '{}': {}. Repair access or select readable source files before preparing migration", access.failed, issue.owner,
                issue.path.as_deref().unwrap_or(Path::new("")).display(), issue.error);
        }
        let mut media = crate::media_staging::MediaStaging::new(staging_parent, destination)?;
        let mut sources: Vec<_> = self
            .dependencies
            .iter()
            .map(|dependency| &dependency.path)
            .collect();
        sources.sort();
        sources.dedup();
        for path in sources {
            control.check()?;
            let source = open_media_source(path)
                .with_context(|| format!("Opening source media {}", path.display()))?;
            media.add(path, &source, control)?;
        }
        let files = self.rewrite_paths(media.paths())?;
        control.check()?;
        media.sync()?;
        control.check()?;
        self.verify_unchanged(&self.source_config_path, &self.working_directory)?;
        control.check()?;
        Ok(PreparedState { media, files })
    }

    pub fn check_assets(
        &self,
        control: &crate::media_staging::CopyControl,
    ) -> Result<AssetAccessReport> {
        control.check()?;
        ensure!(
            self.source_uid == unsafe { libc::geteuid() }
                && self.source_namespace == mount_namespace()?,
            "Saved assets must be checked under the inventoried account and mount namespace"
        );
        self.verify_unchanged(&self.source_config_path, &self.working_directory)?;
        let mut report = AssetAccessReport {
            uid: self.source_uid,
            checked: 0,
            failed: 0,
            issues: Vec::new(),
        };
        let mut checked = std::collections::HashMap::new();
        for dependency in &self.dependencies {
            control.check()?;
            let failure = checked.entry(&dependency.path).or_insert_with(|| {
                open_media_source(&dependency.path)
                    .err()
                    .map(|error| format!("{error:#}"))
            });
            report.checked += 1;
            if let Some(error) = failure {
                report.failed += 1;
                if report.issues.len() < MAX_ISSUES {
                    report.issues.push(AssetAccessIssue {
                        owner: dependency.owner.clone(),
                        path: Some(dependency.path.clone()),
                        error: error.clone(),
                    });
                }
            }
        }
        control.check()?;
        self.verify_unchanged(&self.source_config_path, &self.working_directory)?;
        control.check()?;
        Ok(report)
    }

    /// Produces in-memory state only. Validate staged assets and source generations
    /// before installing it; unchanged source files remain available for rollback.
    pub fn rewrite_paths(
        &self,
        paths: &std::collections::HashMap<PathBuf, PathBuf>,
    ) -> Result<Vec<StateFile>> {
        super::state_rewrite::rewrite(
            &self.files,
            &self.config_directory,
            &self.working_directory,
            paths,
        )
    }

    pub fn verify_unchanged(&self, config_path: &Path, working_directory: &Path) -> Result<()> {
        let current = Self::read(config_path, working_directory)?;
        ensure!(self.summary.generation == current.summary.generation, "State changed after preparation. Leave existing state untouched and prepare the migration again");
        Ok(())
    }
}

fn mount_namespace() -> Result<(u64, u64)> {
    let metadata =
        fs::metadata("/proc/thread-self/ns/mnt").context("Inspecting source mount namespace")?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(crate) fn open_media_source(path: &Path) -> Result<File> {
    let pinned = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(
        pinned.metadata()?.is_file(),
        "Media source is not a regular file"
    );
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
        .context("Opening the pinned media file for reading")
}

fn issue(summary: &mut StateSummary, message: String) {
    summary.issue_count += 1;
    if summary.issues.len() < MAX_ISSUES {
        summary.issues.push(message);
    }
}

fn append_dependencies(
    all: &mut Vec<AssetDependency>,
    found: Vec<AssetDependency>,
    owner: &str,
    working_directory: &Path,
) -> Result<()> {
    validate_dependency_paths(&found).map_err(anyhow::Error::msg)?;
    ensure!(
        all.len() + found.len() <= MAX_DEPENDENCIES,
        "State references more than 4096 media dependencies"
    );
    for mut dependency in found {
        dependency.owner = format!("{owner}: {}", dependency.owner);
        if dependency.path.is_relative() {
            dependency.path = working_directory.join(&dependency.path);
        }
        all.push(dependency);
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(file: &StateFile) -> Result<T> {
    serde_json::from_slice(&file.bytes)
        .with_context(|| format!("Invalid state in {}", file.relative_path.display()))
}

fn hash_directory(hash: &mut Sha256, directory: &File) -> Result<()> {
    let metadata = directory.metadata()?;
    hash.update(metadata.dev().to_le_bytes());
    hash.update(metadata.ino().to_le_bytes());
    Ok(())
}

fn open_entry(directory: &File, name: &OsStr, extra_flags: i32) -> Result<Option<File>> {
    ensure!(
        Path::new(name).file_name() == Some(name),
        "State entry must be a single filename"
    );
    let name = CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | extra_flags,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("Opening state entry {}", name.to_string_lossy()));
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn read_file(
    directory: &File,
    name: &OsStr,
    relative_path: PathBuf,
    total: &mut usize,
) -> Result<Option<StateFile>> {
    let Some(mut file) = open_entry(directory, name, 0)? else {
        return Ok(None);
    };
    let before = file.metadata()?;
    ensure!(
        before.is_file(),
        "{} is not a regular state file",
        relative_path.display()
    );
    let limit = MAX_FILE_BYTES.min(MAX_TOTAL_BYTES.saturating_sub(*total));
    ensure!(
        before.len() <= limit as u64,
        "{} exceeds the state size budget (16 MiB per file, 64 MiB total)",
        relative_path.display()
    );
    let mut bytes = Vec::new();
    (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "{} grew beyond the state size budget",
        relative_path.display()
    );
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "{} changed while being read. Retry",
        relative_path.display()
    );
    *total += bytes.len();
    Ok(Some(StateFile {
        relative_path,
        bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    fn access_control() -> crate::media_staging::CopyControl {
        crate::media_staging::CopyControl::new(std::time::Duration::from_secs(5))
    }

    #[test]
    fn checks_dormant_media_and_template_children_under_the_inventoried_account() {
        let root = tempfile::tempdir().unwrap();
        let working = root.path().join("working");
        fs::create_dir(&working).unwrap();
        let config = root.path().join("config.json");
        fs::write(
            &config,
            br#"{"lcds":[{"index":0,"type":"color","rgb":[0,0,0],"path":"dormant.png"}]}"#,
        )
        .unwrap();
        fs::write(root.path().join("dormant.png"), b"readable").unwrap();
        fs::write(
            root.path().join("lcd_templates.json"),
            serde_json::to_vec(&json!({
                "templates": [{"id":"unused", "name":"Unused", "base_width":400,"base_height":400,
                    "background":{"type":"image","path":"background.png"},
                    "widgets":[{"id":"child", "x":0,"y":0,"width":100,"height":100,
                        "kind":{"type":"video","path":"missing.mp4"}}]}]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(working.join("background.png"), b"readable").unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        fs::write(root.path().join("profiles/Inactive.json"), br#"{"name":"Inactive","device_id":"offline","lcds":[{"index":0,"type":"video","path":"missing.mp4"}]}"#).unwrap();
        let snapshot = StateSnapshot::read(&config, &working).unwrap();
        let report = snapshot.check_assets(&access_control()).unwrap();
        assert_eq!(report.uid, unsafe { libc::geteuid() });
        assert_eq!(report.checked, 4);
        assert_eq!(report.failed, 2);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.owner.contains("widget 'child'")
                && issue.path.as_ref() == Some(&working.join("missing.mp4"))));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.owner.contains("Inactive.json")
                && issue.path.as_ref() == Some(&root.path().join("missing.mp4"))));
        fs::write(working.join("missing.mp4"), b"readable").unwrap();
        fs::write(root.path().join("missing.mp4"), b"readable").unwrap();
        assert_eq!(snapshot.check_assets(&access_control()).unwrap().failed, 0);
        fs::write(&config, b"{}").unwrap();
        assert!(snapshot.check_assets(&access_control()).is_err());
    }

    #[test]
    fn access_reports_are_bounded_and_reject_changed_identity_or_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let lcds: Vec<_> = (0..40)
            .map(|index| {
                json!({
                    "index":index,"type":"image","path":"missing.png"
                })
            })
            .collect();
        fs::write(&config, serde_json::to_vec(&json!({"lcds":lcds})).unwrap()).unwrap();
        let mut snapshot = StateSnapshot::read(&config, root.path()).unwrap();
        let report = snapshot.check_assets(&access_control()).unwrap();
        assert_eq!(report.checked, 40);
        assert_eq!(report.failed, 40);
        assert_eq!(report.issues.len(), 32);
        snapshot.source_namespace.1 = snapshot.source_namespace.1.wrapping_add(1);
        assert!(snapshot.check_assets(&access_control()).is_err());
        snapshot.source_namespace = mount_namespace().unwrap();
        snapshot.source_uid = snapshot.source_uid.wrapping_add(1);
        assert!(snapshot.check_assets(&access_control()).is_err());
        snapshot.source_uid = unsafe { libc::geteuid() };
        let control = access_control();
        control.cancel();
        assert!(snapshot.check_assets(&control).is_err());
    }

    #[test]
    fn preflight_rejects_fifo_assets_without_opening_them_for_io() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(
            &config,
            br#"{"lcds":[{"index":0,"type":"video","path":"pipe"}]}"#,
        )
        .unwrap();
        let pipe = CString::new(root.path().join("pipe").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
        let snapshot = StateSnapshot::read(&config, root.path()).unwrap();
        let report = snapshot.check_assets(&access_control()).unwrap();
        assert_eq!(report.failed, 1);
        assert!(report.issues[0].error.contains("not a regular file"));
    }

    #[test]
    fn invalid_lcd_settings_in_inactive_profiles_block_migration_without_file_access() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        fs::write(
            root.path().join("profiles/Inactive.json"),
            br#"{"name":"Inactive","device_id":"offline","lcds":[{"index":0,"type":"color"}]}"#,
        )
        .unwrap();
        let snapshot = StateSnapshot::read(&config, root.path()).unwrap();
        assert_eq!(snapshot.summary.issue_count, 1);
        assert!(snapshot.summary.issues[0].contains("Inactive.json"));
        assert!(snapshot.summary.issues[0].contains("rgb"));
        assert!(snapshot
            .prepare_media(
                root.path(),
                Path::new("/destination/media"),
                &access_control()
            )
            .is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn retains_raw_state_and_inventories_inactive_profiles_and_templates() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        let raw = br#"{ "hardware_video": false, "future_setting": {"keep": true}, "lcds": [{"type":"image","index":0,"path":"active.png"}] }"#;
        fs::write(&config_path, raw).unwrap();
        fs::write(root.path().join("lcd_templates.json"), serde_json::to_vec(&json!({
            "future_template_setting": 17,
            "templates": [{ "id": "inactive", "name": "Inactive", "base_width": 400, "base_height": 400,
                "background": {"type": "image", "path": "template.png"}, "widgets": [] }]
        })).unwrap()).unwrap();
        fs::write(root.path().join("rgb_presets.json"), b"[]").unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        fs::write(
            root.path().join("profiles/Saved profile.json"),
            serde_json::to_vec(&json!({
                "name": "Saved profile", "device_id": "offline", "future_profile_setting": 42,
                "lcds": [{"type":"video", "index":0, "path":"inactive.mp4"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let snapshot =
            StateSnapshot::read(&config_path, Path::new("/daemon-working-directory")).unwrap();
        assert_eq!(snapshot.summary.files, 4);
        assert_eq!(snapshot.summary.profiles, 1);
        assert_eq!(snapshot.summary.templates, 1);
        assert_eq!(snapshot.summary.issue_count, 0);
        assert_eq!(snapshot.summary.asset_references, 3);
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|file| file.relative_path == Path::new("config.json"))
                .unwrap()
                .bytes,
            raw
        );
        assert!(snapshot
            .dependencies
            .iter()
            .any(|dependency| dependency.path == root.path().join("active.png")));
        assert!(snapshot
            .dependencies
            .iter()
            .any(
                |dependency| dependency.path == root.path().join("inactive.mp4")
                    && dependency.owner.contains("Saved profile.json")
            ));
        assert!(snapshot.dependencies.iter().any(
            |dependency| dependency.path == Path::new("/daemon-working-directory/template.png")
        ));
        snapshot
            .verify_unchanged(&config_path, Path::new("/daemon-working-directory"))
            .unwrap();
        assert!(snapshot
            .verify_unchanged(&config_path, Path::new("/different-working-directory"))
            .is_err());
    }

    #[test]
    fn changed_deleted_and_new_profiles_invalidate_preparation() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        let initial = StateSnapshot::read(&config, root.path()).unwrap();
        let profile = root.path().join("profiles/profile.json");
        fs::write(&profile, br#"{"name":"Saved","device_id":"offline"}"#).unwrap();
        assert!(initial.verify_unchanged(&config, root.path()).is_err());
        let saved = StateSnapshot::read(&config, root.path()).unwrap();
        fs::write(&profile, br#"{"name":"Edited","device_id":"offline"}"#).unwrap();
        assert!(saved.verify_unchanged(&config, root.path()).is_err());
        fs::remove_file(profile).unwrap();
        assert!(saved.verify_unchanged(&config, root.path()).is_err());
    }

    #[test]
    fn corrupt_auxiliary_state_is_not_silently_replaced_with_defaults() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        for name in ["lcd_templates.json", "rgb_presets.json"] {
            let path = root.path().join(name);
            fs::write(&path, b"{broken").unwrap();
            let error = StateSnapshot::read(&config, root.path()).err().unwrap();
            assert!(format!("{error:#}").contains(name));
            fs::remove_file(path).unwrap();
        }
        fs::create_dir(root.path().join("profiles")).unwrap();
        let path = root.path().join("profiles/future.json");
        fs::write(
            &path,
            br#"{"name":"Future","device_id":"offline","schema_version":2}"#,
        )
        .unwrap();
        let error = StateSnapshot::read(&config, root.path()).err().unwrap();
        assert!(error.to_string().contains("unsupported profile schema 2"));
        fs::write(&path, b"broken").unwrap();
        let error = StateSnapshot::read(&config, root.path()).err().unwrap();
        assert!(format!("{error:#}").contains("profiles/future.json"));
    }

    #[test]
    fn rejects_symlinked_state_files_and_profile_directories() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let target = root.path().join("target.json");
        fs::write(&target, b"{}").unwrap();
        symlink(&target, &config).unwrap();
        assert!(StateSnapshot::read(&config, root.path()).is_err());
        fs::remove_file(&config).unwrap();
        fs::write(&config, b"{}").unwrap();
        fs::create_dir(root.path().join("real-profiles")).unwrap();
        symlink(
            root.path().join("real-profiles"),
            root.path().join("profiles"),
        )
        .unwrap();
        assert!(StateSnapshot::read(&config, root.path()).is_err());
    }

    #[test]
    fn rejects_oversized_state_without_reading_its_contents() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        File::create(&config)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        let error = StateSnapshot::read(&config, root.path()).err().unwrap();
        assert!(error.to_string().contains("state size budget"));
    }

    #[test]
    fn stages_media_and_rewrites_state_without_changing_sources() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(
            &config,
            br#"{"lcds":[{"index":0,"type":"image","path":"linked.png"}],"future":true}"#,
        )
        .unwrap();
        fs::write(root.path().join("source.png"), b"source pixels").unwrap();
        symlink(
            root.path().join("source.png"),
            root.path().join("linked.png"),
        )
        .unwrap();
        let snapshot = StateSnapshot::read(&config, root.path()).unwrap();
        let prepared = snapshot
            .prepare_media(
                root.path(),
                Path::new("/destination/media"),
                &crate::media_staging::CopyControl::new(std::time::Duration::from_secs(5)),
            )
            .unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&prepared.files[0].bytes).unwrap();
        let path = Path::new(rewritten["lcds"][0]["path"].as_str().unwrap());
        assert!(path.starts_with("/destination/media"));
        assert_eq!(
            fs::read(prepared.media.directory().join(path.file_name().unwrap())).unwrap(),
            b"source pixels"
        );
        assert_eq!(rewritten["future"], true);
        snapshot.verify_unchanged(&config, root.path()).unwrap();
        drop(prepared);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }

    #[test]
    fn inaccessible_saved_media_aborts_preparation_before_creating_a_stage() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        fs::write(&config, br#"{"lcds":[{"index":0,"type":"image","path":"first.png"},{"index":1,"type":"video","path":"z-missing.mp4"}]}"#).unwrap();
        fs::write(root.path().join("first.png"), b"copied first").unwrap();
        let snapshot = StateSnapshot::read(&config, root.path()).unwrap();
        let control = crate::media_staging::CopyControl::new(std::time::Duration::from_secs(5));
        let error = snapshot
            .prepare_media(root.path(), Path::new("/destination/media"), &control)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("z-missing.mp4"));
        assert_eq!(control.copied_bytes(), 0);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        snapshot.verify_unchanged(&config, root.path()).unwrap();
    }
}
