use crate::state_backups::{self, Locations};
use anyhow::{ensure, Context, Result};
use lianli_shared::backups::BackupTarget;
use lianli_shared::template::catalog::CatalogStorageEntry;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct RuntimeReferences {
    paths: parking_lot::Mutex<RuntimePaths>,
    activity: parking_lot::Mutex<Activity>,
    changed: parking_lot::Condvar,
}

#[derive(Default)]
struct Activity {
    users: usize,
    exclusive: bool,
}

pub struct RuntimeUse<'a>(&'a RuntimeReferences);
impl RuntimeUse<'_> {
    pub fn record(&self, dependencies: &[lianli_shared::media_dependencies::AssetDependency]) {
        self.0.record(dependencies);
    }
}
impl Drop for RuntimeUse<'_> {
    fn drop(&mut self) {
        let mut activity = self.0.activity.lock();
        activity.users -= 1;
        self.0.changed.notify_all();
    }
}

pub struct ExclusiveReview<'a>(&'a RuntimeReferences);
impl Drop for ExclusiveReview<'_> {
    fn drop(&mut self) {
        let mut activity = self.0.activity.lock();
        activity.exclusive = false;
        self.0.changed.notify_all();
    }
}
struct RuntimePaths {
    paths: HashSet<PathBuf>,
    bytes: usize,
    complete: bool,
}

impl Default for RuntimeReferences {
    fn default() -> Self {
        Self {
            paths: parking_lot::Mutex::new(RuntimePaths {
                paths: HashSet::new(),
                bytes: 0,
                complete: true,
            }),
            activity: Default::default(),
            changed: Default::default(),
        }
    }
}

impl RuntimeReferences {
    pub fn enter(&self, check: impl Fn() -> Result<()>) -> Result<RuntimeUse<'_>> {
        loop {
            check()?;
            let mut activity = self.activity.lock();
            if !activity.exclusive && activity.users < 64 {
                activity.users += 1;
                return Ok(RuntimeUse(self));
            }
            self.changed
                .wait_for(&mut activity, Duration::from_millis(100));
        }
    }

    pub fn exclusive_review(&self) -> Result<ExclusiveReview<'_>> {
        let mut activity = self.activity.lock();
        ensure!(
            !activity.exclusive && activity.users == 0,
            "Media preparation, preview or catalog review is active. Retry after it finishes"
        );
        activity.exclusive = true;
        Ok(ExclusiveReview(self))
    }

    fn record(&self, dependencies: &[lianli_shared::media_dependencies::AssetDependency]) {
        for dependency in dependencies {
            if !self.paths.lock().complete {
                return;
            }
            let resolved = dependency.path.canonicalize();
            let mut state = self.paths.lock();
            let Ok(resolved) = resolved else {
                state.complete = false;
                state.paths.clear();
                return;
            };
            for path in [&dependency.path, &resolved] {
                if state.paths.contains(path) {
                    continue;
                }
                let bytes = path.as_os_str().len();
                if bytes > 4096 || state.bytes + bytes > 1024 * 1024 || state.paths.len() >= 4096 {
                    state.complete = false;
                    state.paths.clear();
                    return;
                }
                state.bytes += bytes;
                state.paths.insert(path.clone());
            }
        }
    }

    pub fn inspect(&self, entries: &mut [CatalogStorageEntry]) -> Result<()> {
        let state = self.paths.lock();
        ensure!(state.complete, "Runtime media references are incomplete. Repair media paths and restart the daemon before cleanup");
        let names: HashSet<_> = state
            .paths
            .iter()
            .flat_map(|path| path.components())
            .filter_map(|component| component.as_os_str().to_str())
            .collect();
        for entry in entries {
            entry.runtime_referenced = names.contains(entry.directory.as_str());
            entry.runtime_references_checked = true;
        }
        Ok(())
    }
}

pub fn inspect(locations: &Locations, entries: &mut [CatalogStorageEntry]) -> Result<()> {
    for entry in entries.iter_mut() {
        entry.saved_references.clear();
        entry.saved_reference_count = 0;
        entry.saved_references_checked = false;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let base = locations
        .config
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut sources = Vec::new();
    for (path, target) in [
        (&locations.config, BackupTarget::Configuration),
        (&locations.templates, BackupTarget::Templates),
        (&locations.presets, BackupTarget::RgbPresets),
    ] {
        for suffix in ["", ".bak", ".before-restore"] {
            let mut filename = path.as_os_str().to_owned();
            filename.push(suffix);
            sources.push((PathBuf::from(filename), target.clone()));
        }
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(base.join("profiles"))
    {
        Ok(directory) => {
            for (index, entry) in
                std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?.enumerate()
            {
                ensure!(
                    Instant::now() < deadline,
                    "Saved reference inspection exceeded five seconds"
                );
                ensure!(
                    index < 768,
                    "Profile reference inspection exceeds 768 entries"
                );
                let entry = entry?;
                let filename = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("Profile filename is not UTF-8"))?;
                let original = filename
                    .strip_suffix(".before-restore")
                    .or_else(|| filename.strip_suffix(".bak"))
                    .unwrap_or(&filename);
                if let Some(name) = original.strip_suffix(".json") {
                    ensure!(
                        !name.is_empty() && name.len() <= 250,
                        "Invalid profile reference filename"
                    );
                    sources.push((
                        base.join("profiles").join(&filename),
                        BackupTarget::Profile { name: name.into() },
                    ));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("opening profiles for asset references"),
    }
    let names = entries
        .iter()
        .map(|entry| entry.directory.clone())
        .collect();
    let mut scanner = References {
        names,
        found: HashSet::new(),
        cache: HashMap::new(),
        base: base.to_path_buf(),
        cwd: std::env::current_dir()?,
        profiles: base.join("profiles"),
        path_value: false,
        deadline,
    };
    let mut total = 0usize;
    for (path, target) in sources {
        ensure!(
            Instant::now() < scanner.deadline,
            "Saved reference inspection exceeded five seconds"
        );
        let bytes = match state_backups::read_bytes(&path) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                ensure!(
                    matches!(std::fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                    "State reference file is a dangling link or cannot be inspected"
                );
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting references in {}", path.display()))
            }
        };
        total += bytes.len();
        ensure!(
            total <= 64 * 1024 * 1024,
            "Saved reference inspection exceeds 64 MiB of state"
        );
        state_backups::validate(locations, &target, &bytes)
            .with_context(|| format!("invalid reference state {}", path.display()))?;
        scanner.found.clear();
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
        (&mut scanner)
            .deserialize(&mut deserializer)
            .with_context(|| format!("inspecting asset references in {}", path.display()))?;
        deserializer.end()?;
        let label = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .display()
            .to_string();
        for entry in entries
            .iter_mut()
            .filter(|entry| scanner.found.contains(&entry.directory))
        {
            entry.saved_reference_count += 1;
            if entry.saved_references.len() < 16 {
                entry.saved_references.push(label.clone());
            }
        }
    }
    ensure!(
        Instant::now() < scanner.deadline,
        "Saved reference inspection exceeded five seconds"
    );
    for entry in entries {
        entry.saved_references_checked = true;
    }
    Ok(())
}

struct References {
    names: HashSet<String>,
    found: HashSet<String>,
    cache: HashMap<String, (HashSet<String>, bool)>,
    base: PathBuf,
    cwd: PathBuf,
    profiles: PathBuf,
    path_value: bool,
    deadline: Instant,
}

impl References {
    fn record(&mut self, value: &str) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "Saved reference inspection exceeded five seconds"
        );
        let mut found = HashSet::new();
        for component in value.split('/') {
            if self.names.contains(component) {
                found.insert(component.to_owned());
            }
        }
        if value.is_empty() || value.len() > 4096 {
            ensure!(
                !self.path_value,
                "A saved asset path is empty or exceeds 4096 bytes"
            );
            self.found.extend(found);
            return Ok(());
        }
        if let Some((found, resolved)) = self.cache.get(value) {
            ensure!(
                !self.path_value || *resolved || !found.is_empty(),
                "Saved asset path '{value}' cannot be resolved. Reference inspection is incomplete"
            );
            self.found.extend(found.iter().cloned());
            return Ok(());
        }
        ensure!(
            self.cache.len() < 8192,
            "Saved reference inspection exceeds 8192 distinct strings"
        );
        let mut resolved = false;
        for base in [&self.base, &self.cwd, &self.profiles] {
            if let Ok(path) = base.join(value).canonicalize() {
                resolved = true;
                for component in path.components() {
                    if let Some(component) = component
                        .as_os_str()
                        .to_str()
                        .filter(|component| self.names.contains(*component))
                    {
                        found.insert(component.to_owned());
                    }
                }
            }
        }
        ensure!(
            !self.path_value || resolved || !found.is_empty(),
            "Saved asset path '{value}' cannot be resolved. Reference inspection is incomplete"
        );
        self.found.extend(found.iter().cloned());
        self.cache.insert(value.to_owned(), (found, resolved));
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for &mut References {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(ReferenceVisitor(self))
    }
}

struct ReferenceVisitor<'a>(&'a mut References);
impl<'de> Visitor<'de> for ReferenceVisitor<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("JSON state")
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<(), E> {
        self.0.record(value).map_err(E::custom)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence.next_element_seed(&mut *self.0)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            let previous = self.0.path_value;
            self.0.path_value = false;
            self.0.record(&key).map_err(serde::de::Error::custom)?;
            self.0.path_value = key == "path" || (key.ends_with("_path") && key != "device_path");
            map.next_value_seed(&mut *self.0)?;
            self.0.path_value = previous;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_review_refuses_active_use_and_wakes_waiting_preparation_on_release() {
        let runtime = std::sync::Arc::new(RuntimeReferences::default());
        let usage = runtime.enter(|| Ok(())).unwrap();
        assert!(runtime.exclusive_review().is_err());
        drop(usage);
        let exclusive = runtime.exclusive_review().unwrap();
        let (checking, checked) = std::sync::mpsc::sync_channel(1);
        let (finished, done) = std::sync::mpsc::sync_channel(1);
        let worker_runtime = runtime.clone();
        let worker = std::thread::spawn(move || {
            let _usage = worker_runtime
                .enter(|| {
                    let _ = checking.try_send(());
                    Ok(())
                })
                .unwrap();
            finished.send(()).unwrap();
        });
        checked.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(runtime.activity.lock().users, 0);
        assert!(done.try_recv().is_err());
        drop(exclusive);
        done.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        assert!(runtime.exclusive_review().is_ok());
    }

    #[test]
    fn waiting_preparation_honors_cancellation_without_releasing_exclusive_ownership() {
        let runtime = RuntimeReferences::default();
        let exclusive = runtime.exclusive_review().unwrap();
        let checks = std::cell::Cell::new(0);
        assert!(runtime
            .enter(|| {
                checks.set(checks.get() + 1);
                anyhow::ensure!(checks.get() == 1, "cancelled");
                Ok(())
            })
            .is_err());
        assert_eq!(checks.get(), 2);
        assert!(runtime.exclusive_review().is_err());
        drop(exclusive);
        let users: Vec<_> = (0..64).map(|_| runtime.enter(|| Ok(())).unwrap()).collect();
        assert_eq!(runtime.activity.lock().users, 64);
        assert!(runtime.enter(|| anyhow::bail!("cancelled")).is_err());
        drop(users);
        assert!(runtime.exclusive_review().is_ok());
    }

    fn dependency(path: PathBuf) -> lianli_shared::media_dependencies::AssetDependency {
        lianli_shared::media_dependencies::AssetDependency {
            path,
            owner: "fixture".into(),
            kind: lianli_shared::media_dependencies::AssetKind::Image,
        }
    }

    #[test]
    fn runtime_retains_prior_alias_targets_until_the_daemon_session_ends() {
        let root = tempfile::tempdir().unwrap();
        let names = ["catalog-old-ABC123", "catalog-new-DEF456"];
        for name in names {
            std::fs::create_dir(root.path().join(name)).unwrap();
            std::fs::write(root.path().join(name).join("asset"), b"x").unwrap();
        }
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join(names[0]).join("asset"), &alias).unwrap();
        let runtime = RuntimeReferences::default();
        runtime.record(&[dependency(alias.clone())]);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(root.path().join(names[1]).join("asset"), &alias).unwrap();
        runtime.record(&[dependency(alias)]);
        let mut entries: Vec<CatalogStorageEntry> = serde_json::from_value(serde_json::json!(names.map(|name| serde_json::json!({"directory":name,"bytes":1,"ownership_verified":false,"issue":null})))).unwrap();
        runtime.inspect(&mut entries).unwrap();
        assert!(entries
            .iter()
            .all(|entry| entry.runtime_referenced && entry.runtime_references_checked));
        RuntimeReferences::default().inspect(&mut entries).unwrap();
        assert!(entries.iter().all(|entry| !entry.runtime_referenced));
    }

    #[test]
    fn runtime_failures_and_capacity_limits_never_reenable_cleanup_assessment() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset");
        std::fs::write(&path, b"x").unwrap();
        let missing = RuntimeReferences::default();
        missing.record(&[dependency(root.path().join("missing"))]);
        missing.record(&[dependency(path.clone())]);
        assert!(missing.inspect(&mut []).is_err());
        for full_count in [false, true] {
            let runtime = RuntimeReferences::default();
            {
                let mut state = runtime.paths.lock();
                if full_count {
                    state
                        .paths
                        .extend((0..4096).map(|index| PathBuf::from(index.to_string())));
                } else {
                    state.bytes = 1024 * 1024;
                }
            }
            runtime.record(&[dependency(path.clone())]);
            assert!(runtime.inspect(&mut []).is_err());
            assert!(runtime.paths.lock().paths.is_empty());
        }
    }

    #[test]
    fn reference_scan_preserves_direct_child_profile_and_backup_sources_including_aliases() {
        let root = tempfile::tempdir().unwrap();
        let name = "catalog-cooler-ABC123";
        let asset = root.path().join("templates").join(name).join("image.png");
        std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
        std::fs::write(&asset, b"fixture").unwrap();
        let alias = root.path().join("alias.png");
        std::os::unix::fs::symlink(&asset, &alias).unwrap();
        let locations = Locations {
            config: root.path().join("config.json"),
            templates: root.path().join("lcd_templates.json"),
            presets: root.path().join("rgb_presets.json"),
        };
        let lcd = serde_json::json!({"index":0,"type":"image","path":alias});
        let config = serde_json::to_vec(&serde_json::json!({"lcds":[lcd.clone()]})).unwrap();
        for suffix in ["", ".bak", ".before-restore"] {
            std::fs::write(root.path().join(format!("config.json{suffix}")), &config).unwrap();
        }
        let mut template: lianli_shared::template::LcdTemplate = serde_json::from_str(
            include_str!("../../../templates/assets/cooler/template.json"),
        )
        .unwrap();
        lianli_shared::media_dependencies::map_template_paths(&mut template, |path| {
            *path = asset.clone()
        });
        std::fs::write(
            &locations.templates,
            serde_json::to_vec(&serde_json::json!({"templates":[template]})).unwrap(),
        )
        .unwrap();
        std::fs::create_dir(root.path().join("profiles")).unwrap();
        let profile = serde_json::to_vec(
            &serde_json::json!({"name":"gaming","device_id":"fixture","lcds":[lcd]}),
        )
        .unwrap();
        for suffix in ["", ".bak", ".before-restore"] {
            std::fs::write(
                root.path().join(format!("profiles/gaming.json{suffix}")),
                &profile,
            )
            .unwrap();
        }
        let mut entries: Vec<CatalogStorageEntry> = serde_json::from_value(serde_json::json!([{"directory":name,"bytes":7,"ownership_verified":false,"issue":null}])).unwrap();
        inspect(&locations, &mut entries).unwrap();
        assert!(entries[0].saved_references_checked);
        assert_eq!(entries[0].saved_reference_count, 7);
        assert!(entries[0]
            .saved_references
            .contains(&"profiles/gaming.json.before-restore".into()));
        assert_eq!(std::fs::read(&asset).unwrap(), b"fixture");
        std::fs::write(&locations.templates, b"{").unwrap();
        assert!(inspect(&locations, &mut entries).is_err());
        assert!(!entries[0].saved_references_checked);
        assert_eq!(std::fs::read(&locations.templates).unwrap(), b"{");
    }

    #[test]
    fn unresolved_asset_aliases_and_symlink_state_files_block_reference_completion() {
        let root = tempfile::tempdir().unwrap();
        let locations = Locations {
            config: root.path().join("config.json"),
            templates: root.path().join("lcd_templates.json"),
            presets: root.path().join("rgb_presets.json"),
        };
        std::fs::write(
            &locations.config,
            br#"{"lcds":[{"index":0,"type":"image","path":"missing.png"}]}"#,
        )
        .unwrap();
        let error = inspect(&locations, &mut []).unwrap_err();
        assert!(format!("{error:#}").contains("missing.png"));
        std::fs::remove_file(&locations.config).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing.json"), &locations.config).unwrap();
        assert!(inspect(&locations, &mut []).is_err());
    }

    #[test]
    fn sensor_device_identifiers_do_not_require_media_files() {
        let root = tempfile::tempdir().unwrap();
        let locations = Locations {
            config: root.path().join("config.json"),
            templates: root.path().join("lcd_templates.json"),
            presets: root.path().join("rgb_presets.json"),
        };
        std::fs::write(&locations.config, br#"{"lcds":[{"index":0,"type":"sensor","sensor":{"label":"Temperature","unit":"C","source":{"type":"hwmon","name":"asus-ec-sensors","label":"CPU","device_path":"asus-ec-sensors"}}}]}"#).unwrap();
        inspect(&locations, &mut []).unwrap();
    }
}
