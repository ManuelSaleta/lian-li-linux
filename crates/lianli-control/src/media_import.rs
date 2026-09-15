use crate::media_staging::{CopyControl, MediaStaging};
use anyhow::{ensure, Context, Result};
use lianli_shared::config::LcdConfig;
use lianli_shared::media_dependencies::{self, AssetDependency};
use lianli_shared::template::LcdTemplate;
use std::path::Path;

struct SelectionSize(usize, usize);
impl std::io::Write for SelectionSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.1 - self.0 {
            return Err(std::io::Error::other(
                "Managed media selection exceeds its size limit",
            ));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct PreparedSelection {
    pub lcds: Vec<LcdConfig>,
    pub templates: Vec<LcdTemplate>,
    pub media: MediaStaging,
}

pub struct ValidatedSelection {
    selection: PreparedSelection,
    fingerprint: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishedSelection {
    pub import_id: String,
    pub lcds: Vec<LcdConfig>,
    pub templates: Vec<LcdTemplate>,
}

impl ValidatedSelection {
    pub fn selection(&self) -> &PreparedSelection {
        &self.selection
    }

    /// The caller must hold exclusive import/write admission for this destination.
    pub fn publish(
        self,
        config_dir: &Path,
        import_id: &str,
        control: &CopyControl,
    ) -> Result<PublishedSelection> {
        use crate::media_publication::{self, Directory};
        use std::os::fd::AsRawFd;
        control.check()?;
        serde_json::to_writer(
            SelectionSize(0, 16 * 1024 * 1024 - 128),
            &(&self.selection.lcds, &self.selection.templates),
        )?;
        ensure!(
            import_id.len() == 32 && import_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid managed import ID"
        );
        let destination = config_dir
            .canonicalize()?
            .join("media/imports")
            .join(import_id);
        ensure!(
            self.selection
                .media
                .paths()
                .values()
                .all(|path| path.parent() == Some(destination.as_path())),
            "Staged selection targets a different managed import destination"
        );
        let root = Directory::open(config_dir)?;
        let media_directory = root.child("media", true)?;
        let imports = media_directory.child("imports", true)?;
        let source = Directory::open(self.selection.media.directory())?;
        media_publication::check_capacity(&imports, &source, 8 * 1024 * 1024 * 1024)?;
        ensure!(
            media_publication::fingerprint_directory(&source)? == self.fingerprint,
            "Staged media changed after decoder validation"
        );
        let parent = Directory::open(
            &self
                .selection
                .media
                .directory()
                .parent()
                .context("Missing staging parent")?
                .join("."),
        )?;
        let name = std::ffi::CString::new(
            self.selection
                .media
                .directory()
                .file_name()
                .context("Missing staging name")?
                .as_encoded_bytes(),
        )?;
        let target = std::ffi::CString::new(import_id)?;
        control.check()?;
        crate::media_ownership::record(
            &media_directory,
            &imports,
            &source,
            import_id,
            &self.fingerprint,
        )?;
        control.check()?;
        ensure!(
            unsafe {
                libc::renameat2(
                    parent.0.as_raw_fd(),
                    name.as_ptr(),
                    imports.0.as_raw_fd(),
                    target.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } == 0,
            "Publishing managed selection: {}",
            std::io::Error::last_os_error()
        );
        let PreparedSelection {
            lcds,
            templates,
            media,
        } = self.selection;
        media.published();
        let complete = (|| -> Result<()> {
            ensure!(
                media_publication::fingerprint_directory(&imports.child(import_id, false)?)?
                    == self.fingerprint,
                "Published media changed before completion"
            );
            parent.0.sync_all()?;
            imports.0.sync_all()?;
            Ok(())
        })();
        complete.context("Managed files were moved but completion could not be verified; retain the import and inspect storage before retrying")?;
        Ok(PublishedSelection {
            import_id: import_id.into(),
            lcds,
            templates,
        })
    }
}

impl PreparedSelection {
    /// Validate the staged copies under the destination account before publication.
    pub fn validate(self, control: &CopyControl) -> Result<ValidatedSelection> {
        self.validate_with(control, |dependencies| {
            crate::media_validation::ensure_decodable_checked(dependencies, || control.check())
        })
    }

    fn validate_with(
        self,
        control: &CopyControl,
        decode: impl FnOnce(&[AssetDependency]) -> Result<()>,
    ) -> Result<ValidatedSelection> {
        control.check()?;
        let mut dependencies = Vec::new();
        for lcd in &self.lcds {
            dependencies.extend(
                media_dependencies::lcd_dependencies(lcd, &self.templates)
                    .map_err(anyhow::Error::msg)?,
            );
            dependencies.extend(media_dependencies::stored_lcd_dependencies(lcd));
        }
        for template in &self.templates {
            dependencies.extend(media_dependencies::template_dependencies(template));
        }
        ensure!(
            dependencies.len() <= 4096,
            "Managed validation exceeds 4096 references"
        );
        let destinations: std::collections::HashSet<_> = self.media.paths().values().collect();
        for dependency in &mut dependencies {
            ensure!(
                destinations.contains(&dependency.path),
                "Selection references media outside this staged import"
            );
            let name = dependency
                .path
                .file_name()
                .context("Staged media filename is missing")?;
            dependency.path = self.media.directory().join(name);
        }
        let fingerprint = crate::media_publication::fingerprint(self.media.directory())?;
        decode(&dependencies)?;
        control.check()?;
        ensure!(
            crate::media_publication::fingerprint(self.media.directory())? == fingerprint,
            "Staged media changed during validation"
        );
        Ok(ValidatedSelection {
            selection: self,
            fingerprint,
        })
    }
}

/// Run under the source account in a supervised worker. Returned paths are not published yet.
pub fn prepare_selection(
    lcds: Vec<LcdConfig>,
    templates: Vec<LcdTemplate>,
    staging_parent: &Path,
    destination: &Path,
    control: &CopyControl,
) -> Result<PreparedSelection> {
    control.check()?;
    prepare_from(
        lcds,
        templates,
        staging_parent,
        destination,
        control,
        crate::state::open_media_source,
    )
}

pub(crate) fn selection_dependencies(
    lcds: &[LcdConfig],
    templates: &[LcdTemplate],
) -> Result<Vec<AssetDependency>> {
    selection_dependencies_with_limit(lcds, templates, 1024 * 1024)
}

pub(crate) fn selection_dependencies_with_limit(
    lcds: &[LcdConfig],
    templates: &[LcdTemplate],
    limit: usize,
) -> Result<Vec<AssetDependency>> {
    serde_json::to_writer(SelectionSize(0, limit), &(lcds, templates))?;
    media_dependencies::validate_dependency_input(lcds, templates).map_err(anyhow::Error::msg)?;
    let mut dependencies: Vec<AssetDependency> = Vec::new();
    for lcd in lcds {
        dependencies.extend(
            media_dependencies::lcd_dependencies(lcd, templates).map_err(anyhow::Error::msg)?,
        );
        dependencies.extend(media_dependencies::stored_lcd_dependencies(lcd));
        ensure!(
            dependencies.len() <= 4096,
            "Managed media selection exceeds 4096 asset references"
        );
    }
    for template in templates {
        dependencies.extend(media_dependencies::template_dependencies(template));
        ensure!(
            dependencies.len() <= 4096,
            "Managed media selection exceeds 4096 asset references"
        );
    }
    media_dependencies::validate_dependency_paths(&dependencies).map_err(anyhow::Error::msg)?;
    ensure!(
        !dependencies.is_empty(),
        "This selection has no file assets to copy"
    );
    ensure!(
        dependencies
            .iter()
            .all(|dependency| dependency.path.is_absolute()),
        "Select absolute media paths before copying into managed storage"
    );
    dependencies.sort_by(|a, b| a.path.cmp(&b.path));
    dependencies.dedup_by(|a, b| a.path == b.path);
    Ok(dependencies)
}

pub(crate) fn prepare_from(
    mut lcds: Vec<LcdConfig>,
    mut templates: Vec<LcdTemplate>,
    staging_parent: &Path,
    destination: &Path,
    control: &CopyControl,
    mut open: impl FnMut(&Path) -> Result<std::fs::File>,
) -> Result<PreparedSelection> {
    control.check()?;
    let dependencies = selection_dependencies(&lcds, &templates)?;
    let mut media = MediaStaging::new(staging_parent, destination)?;
    for dependency in dependencies {
        control.check()?;
        let source = open(&dependency.path).with_context(|| {
            format!(
                "Opening {}: {}",
                dependency.owner,
                dependency.path.display()
            )
        })?;
        media.add(&dependency.path, &source, control)?;
    }
    let replace = |path: &mut std::path::PathBuf| {
        if let Some(destination) = media.paths().get(path) {
            *path = destination.clone();
        }
    };
    for lcd in &mut lcds {
        media_dependencies::map_lcd_paths(lcd, replace);
    }
    for template in &mut templates {
        media_dependencies::map_template_paths(template, replace);
    }
    control.check()?;
    media.sync()?;
    control.check()?;
    Ok(PreparedSelection {
        lcds,
        templates,
        media,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    fn validated(root: &Path, id: &str, bytes: &[u8], control: &CopyControl) -> ValidatedSelection {
        let source = root.join("original.png");
        fs::write(&source, bytes).unwrap();
        let lcd =
            serde_json::from_value(serde_json::json!({"type":"image", "path":source})).unwrap();
        prepare_selection(
            vec![lcd],
            vec![],
            root,
            &root.canonicalize().unwrap().join("media/imports").join(id),
            control,
        )
        .unwrap()
        .validate_with(control, |_| Ok(()))
        .unwrap()
    }

    #[test]
    fn publication_returns_durable_paths_and_never_overwrites_a_prior_import() {
        let root = tempfile::tempdir().unwrap();
        let id = "1234567890abcdef1234567890abcdef";
        let control = CopyControl::new(Duration::from_secs(3));
        let first = validated(root.path(), id, b"first", &control);
        let stage = first.selection().media.directory().to_path_buf();
        let published = first.publish(root.path(), id, &control).unwrap();
        assert!(!stage.exists());
        let path = published.lcds[0].path.as_ref().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"first");
        assert!(crate::media_ownership::verify(root.path(), id).unwrap());
        let inventory = crate::media_ownership::inspect(root.path()).unwrap();
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].bytes, 5);
        assert!(inventory[0].ownership_verified);
        let reviewed = crate::media_ownership::review(root.path(), id).unwrap();
        assert_eq!(reviewed.bytes, 5);
        assert!(reviewed
            .files
            .iter()
            .all(|file| file.matches_catalog == Some(true)));
        assert!(reviewed.files.iter().all(|file| file.bytes == 5));
        let second = validated(root.path(), id, b"second", &control);
        assert!(second.publish(root.path(), id, &control).is_err());
        assert_eq!(fs::read(path).unwrap(), b"first");
        assert_eq!(
            fs::read(root.path().join("original.png")).unwrap(),
            b"second"
        );
        assert!(crate::media_ownership::verify(root.path(), id).unwrap());
        fs::write(path, b"altered").unwrap();
        assert!(crate::media_ownership::verify(root.path(), id).is_err());
    }

    #[test]
    fn managed_removal_recovers_each_unlink_and_metadata_retirement_boundary() {
        for stop_after in 1..=4 {
            let root = tempfile::tempdir().unwrap();
            let id = "1234567890abcdef1234567890abcdef";
            let control = CopyControl::new(Duration::from_secs(3));
            validated(root.path(), id, b"first", &control)
                .publish(root.path(), id, &control)
                .unwrap();
            let review = crate::media_ownership::review(root.path(), id).unwrap();
            assert_eq!(review.files.len(), 2);
            let mut steps = 0;
            let result = crate::media_removal::remove_with(
                root.path(),
                id,
                &review.sha256,
                || Ok(()),
                || {
                    steps += 1;
                    anyhow::ensure!(steps != stop_after, "Injected interruption");
                    Ok(())
                },
            );
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("Injected interruption"));
            let resumed = crate::media_ownership::review(root.path(), id).unwrap();
            assert!(!resumed.missing_files.is_empty());
            let inventory = crate::media_ownership::inspect(root.path()).unwrap();
            assert_eq!(inventory.len(), 1);
            assert!(inventory[0].ownership_verified);
            assert!(inventory[0]
                .issue
                .as_ref()
                .unwrap()
                .contains("Interrupted removal"));
            assert!(crate::media_removal::remove(
                root.path(),
                id,
                &resumed.sha256,
                || anyhow::bail!("New saved reference")
            )
            .is_err());
            assert!(root
                .path()
                .join(format!("media/removals/{id}.json"))
                .exists());
            crate::media_removal::remove(root.path(), id, &resumed.sha256, || Ok(())).unwrap();
            assert!(!root.path().join("media/imports").join(id).exists());
            assert!(!root
                .path()
                .join(format!("media/receipts/{id}.json"))
                .exists());
            assert!(!root
                .path()
                .join(format!("media/removals/{id}.json"))
                .exists());
            assert_eq!(
                fs::read(root.path().join("original.png")).unwrap(),
                b"first"
            );
        }
    }

    #[test]
    fn removal_record_survives_alias_unlink_and_refuses_replacements() {
        let root = tempfile::tempdir().unwrap();
        let id = "1234567890abcdef1234567890abcdef";
        let control = CopyControl::new(Duration::from_secs(3));
        validated(root.path(), id, b"first", &control)
            .publish(root.path(), id, &control)
            .unwrap();
        let review = crate::media_ownership::review(root.path(), id).unwrap();
        let prepared = crate::media_removal::prepare(root.path(), &review).unwrap();
        prepared.verify_review(&review).unwrap();
        let directory = root.path().join("media/imports").join(id);
        let alias = review
            .files
            .iter()
            .find(|file| file.path.contains('.'))
            .unwrap();
        fs::remove_file(directory.join(&alias.path)).unwrap();
        assert!(crate::media_ownership::verify(root.path(), id).is_err());
        drop(prepared);
        let resumed = crate::media_removal::resume(root.path(), id)
            .unwrap()
            .unwrap();
        resumed.verify_remaining().unwrap();
        assert!(resumed.verify_review(&review).is_err());
        let object = review
            .files
            .iter()
            .find(|file| !file.path.contains('.'))
            .unwrap();
        let object_path = directory.join(&object.path);
        let modified = fs::metadata(&object_path).unwrap().modified().unwrap();
        fs::write(&object_path, b"other").unwrap();
        fs::File::open(&object_path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(crate::media_removal::resume(root.path(), id)
            .unwrap()
            .unwrap()
            .review()
            .is_err());
        fs::write(&object_path, b"first").unwrap();
        fs::File::open(&object_path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        fs::rename(&object_path, root.path().join("retained-original")).unwrap();
        fs::write(&object_path, b"first").unwrap();
        fs::set_permissions(
            &object_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        assert!(crate::media_removal::resume(root.path(), id).is_err());
        assert_eq!(fs::read(object_path).unwrap(), b"first");
        assert!(root
            .path()
            .join(format!("media/removals/{id}.json"))
            .exists());
    }

    #[test]
    fn removal_preparation_rejects_incomplete_or_unsafe_reviews() {
        let root = tempfile::tempdir().unwrap();
        let id = "1234567890abcdef1234567890abcdef";
        let control = CopyControl::new(Duration::from_secs(3));
        validated(root.path(), id, b"first", &control)
            .publish(root.path(), id, &control)
            .unwrap();
        let review = crate::media_ownership::review(root.path(), id).unwrap();
        let mut unsafe_review = review.clone();
        unsafe_review.files[0].path = "../../original.png".into();
        assert!(crate::media_removal::prepare(root.path(), &unsafe_review).is_err());
        assert!(!root.path().join("media/removals").exists());
        let mut incomplete = review;
        incomplete.files.clear();
        assert!(crate::media_removal::prepare(root.path(), &incomplete).is_err());
        assert_eq!(
            fs::read(root.path().join("original.png")).unwrap(),
            b"first"
        );
        assert!(crate::media_ownership::verify(root.path(), id).unwrap());
    }

    #[test]
    fn inventory_exposes_retained_receipts_for_metadata_review() {
        let root = tempfile::tempdir().unwrap();
        let id = "1234567890abcdef1234567890abcdef";
        let control = CopyControl::new(Duration::from_secs(3));
        validated(root.path(), id, b"first", &control)
            .publish(root.path(), id, &control)
            .unwrap();
        let imports = root.path().join("media/imports");
        fs::remove_dir_all(imports.join(id)).unwrap();
        for _ in 0..2 {
            let inventory = crate::media_ownership::inspect(root.path()).unwrap();
            assert_eq!(inventory.len(), 1);
            assert_eq!(inventory[0].directory, id);
            assert_eq!(inventory[0].bytes, 0);
            assert!(inventory[0].ownership_verified);
            assert!(inventory[0]
                .issue
                .as_ref()
                .unwrap()
                .contains("no published directory"));
            let review = crate::media_ownership::review(root.path(), id).unwrap();
            assert!(review.files.is_empty());
            assert_eq!(review.bytes, 0);
            if imports.exists() {
                fs::remove_dir(&imports).unwrap();
            }
        }
        let receipt = root.path().join(format!("media/receipts/{id}.json"));
        fs::write(&receipt, b"invalid").unwrap();
        let inventory = crate::media_ownership::inspect(root.path()).unwrap();
        assert!(!inventory[0].ownership_verified);
        assert!(inventory[0]
            .issue
            .as_ref()
            .unwrap()
            .contains("Invalid retained ownership receipt"));
        assert_eq!(fs::read(receipt).unwrap(), b"invalid");
    }

    #[test]
    fn orphan_receipt_removal_requires_fresh_review_and_reference_authorization() {
        for missing_store in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let id = "1234567890abcdef1234567890abcdef";
            let control = CopyControl::new(Duration::from_secs(3));
            validated(root.path(), id, b"first", &control)
                .publish(root.path(), id, &control)
                .unwrap();
            let imports = root.path().join("media/imports");
            fs::remove_dir_all(imports.join(id)).unwrap();
            if missing_store {
                fs::remove_dir(&imports).unwrap();
            }
            let review = crate::media_ownership::review(root.path(), id).unwrap();
            let receipt = root.path().join(format!("media/receipts/{id}.json"));
            assert!(crate::media_removal::remove(
                root.path(),
                id,
                &review.sha256,
                || anyhow::bail!("Referenced")
            )
            .is_err());
            assert!(receipt.exists());
            assert!(crate::media_removal::remove(root.path(), id, "stale", || Ok(())).is_err());
            assert!(
                crate::media_removal::remove(root.path(), id, &review.sha256, || {
                    fs::create_dir_all(imports.join(id))?;
                    fs::write(imports.join(id).join("replacement"), b"preserve")?;
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(
                fs::read(imports.join(id).join("replacement")).unwrap(),
                b"preserve"
            );
            assert!(receipt.exists());
            fs::remove_dir_all(imports.join(id)).unwrap();
            crate::media_removal::remove(root.path(), id, &review.sha256, || Ok(())).unwrap();
            assert!(!receipt.exists());
            assert_eq!(
                fs::read(root.path().join("original.png")).unwrap(),
                b"first"
            );
            assert!(crate::media_ownership::inspect(root.path())
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn changed_validated_copies_are_not_published() {
        let root = tempfile::tempdir().unwrap();
        let id = "abcdef1234567890abcdef1234567890";
        let control = CopyControl::new(Duration::from_secs(3));
        let validated = validated(root.path(), id, b"original", &control);
        let stage = validated.selection().media.directory().to_path_buf();
        let file = fs::read_dir(&stage)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(file, b"modified").unwrap();
        assert!(validated.publish(root.path(), id, &control).is_err());
        assert!(!stage.exists());
        assert!(!root.path().join("media/imports").join(id).exists());
        assert_eq!(
            fs::read(root.path().join("original.png")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn selection_copies_and_rewrites_direct_and_template_assets_without_touching_sources() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("picture.png");
        fs::write(&source, b"shared content").unwrap();
        let lcd =
            serde_json::from_value(serde_json::json!({"type":"image", "path":source})).unwrap();
        let template = serde_json::from_value(serde_json::json!({
            "id":"draft", "name":"Draft", "base_width":400, "base_height":400,
            "background":{"type":"image", "path":source}, "widgets":[]
        }))
        .unwrap();
        let control = CopyControl::new(Duration::from_secs(3));
        let prepared = prepare_selection(
            vec![lcd],
            vec![template],
            root.path(),
            Path::new("/managed/import"),
            &control,
        )
        .unwrap();
        assert_eq!(prepared.media.unique_files(), 1);
        let copied = prepared.lcds[0].path.as_ref().unwrap();
        assert!(copied.starts_with("/managed/import"));
        assert_eq!(
            &media_dependencies::template_dependencies(&prepared.templates[0])[0].path,
            copied
        );
        assert_eq!(
            fs::read(prepared.media.directory().join(copied.file_name().unwrap())).unwrap(),
            b"shared content"
        );
        let staged = prepared.media.directory().to_path_buf();
        let validated = prepared
            .validate_with(&control, |dependencies| {
                ensure!(
                    dependencies
                        .iter()
                        .all(|dependency| dependency.path.starts_with(&staged)),
                    "Validation must read staged copies"
                );
                ensure!(
                    dependencies
                        .iter()
                        .all(|dependency| std::fs::read(&dependency.path).unwrap()
                            == b"shared content"),
                    "Staged bytes changed"
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(validated.selection().media.unique_files(), 1);
        drop(validated);
        assert!(!staged.exists());
        assert_eq!(fs::read(source).unwrap(), b"shared content");
    }

    #[test]
    fn rejected_or_cancelled_selections_do_not_leave_staged_copies() {
        let root = tempfile::tempdir().unwrap();
        let lcd: LcdConfig =
            serde_json::from_value(serde_json::json!({"type":"image", "path":"relative.png"}))
                .unwrap();
        let control = CopyControl::new(Duration::from_secs(3));
        assert!(prepare_selection(
            vec![lcd.clone()],
            vec![],
            root.path(),
            Path::new("/managed"),
            &control
        )
        .is_err());
        control.cancel();
        assert!(prepare_selection(
            vec![lcd],
            vec![],
            root.path(),
            Path::new("/managed"),
            &control
        )
        .is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_missing_child_discards_prior_copies_and_large_input_is_rejected_before_staging() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a.png");
        fs::write(&source, b"preserve").unwrap();
        let lcd: LcdConfig =
            serde_json::from_value(serde_json::json!({"type":"image", "path":source})).unwrap();
        let missing = serde_json::from_value(
            serde_json::json!({"type":"image", "path":root.path().join("z.png")}),
        )
        .unwrap();
        let control = CopyControl::new(Duration::from_secs(3));
        assert!(prepare_selection(
            vec![lcd.clone(), missing],
            vec![],
            root.path(),
            Path::new("/managed"),
            &control
        )
        .is_err());
        let mut oversized = lcd;
        oversized.path = Some(format!("/{}", "x".repeat(1024 * 1024)).into());
        assert!(prepare_selection(
            vec![oversized],
            vec![],
            root.path(),
            Path::new("/managed"),
            &control
        )
        .is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(fs::read(source).unwrap(), b"preserve");
    }

    #[test]
    fn decoder_rejection_discards_staging_and_never_reopens_the_original_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.png");
        fs::write(&source, b"invalid image").unwrap();
        let lcd =
            serde_json::from_value(serde_json::json!({"type":"image", "path":source})).unwrap();
        let control = CopyControl::new(Duration::from_secs(3));
        let prepared = prepare_selection(
            vec![lcd],
            vec![],
            root.path(),
            Path::new("/managed"),
            &control,
        )
        .unwrap();
        let staged = prepared.media.directory().to_path_buf();
        fs::write(&source, b"new source").unwrap();
        let failure = prepared.validate_with(&control, |dependencies| {
            assert!(dependencies
                .iter()
                .any(|dependency| dependency.kind == media_dependencies::AssetKind::Image));
            assert!(dependencies
                .iter()
                .all(|dependency| fs::read(&dependency.path).unwrap() == b"invalid image"));
            anyhow::bail!("Image decoder rejected the staged file")
        });
        assert!(failure.is_err());
        assert!(!staged.exists());
        assert_eq!(fs::read(source).unwrap(), b"new source");
    }
}
