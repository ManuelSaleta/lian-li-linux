use super::SharedState;
use crate::catalog_references::RuntimeReferences;
use crate::state_backups::Locations;
use lianli_shared::ipc::IpcResponse;
use lianli_shared::template::catalog::{
    self, CatalogCleanupReview, CatalogReviewStatus, CatalogStorageEntry,
};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

static NEXT_REVIEW: AtomicU64 = AtomicU64::new(1);
type Job = Arc<Mutex<Option<CatalogReviewStatus>>>;

fn reserve(job: &Job, instance: &str) -> Result<CatalogReviewStatus, IpcResponse> {
    let mut slot = job.lock();
    if slot.as_ref().is_some_and(|status| !status.finished) {
        return Err(IpcResponse::error(
            "A media content review is already running. Wait before reviewing another directory",
        ));
    }
    let status = CatalogReviewStatus {
        operation_id: format!("{instance}:{}", NEXT_REVIEW.fetch_add(1, Ordering::Relaxed)),
        finished: false,
        review: None,
        error: None,
        removed: false,
    };
    *slot = Some(status.clone());
    Ok(status)
}

fn finish(job: &Job, result: anyhow::Result<CatalogCleanupReview>) {
    if let Some(status) = job.lock().as_mut() {
        status.finished = true;
        match result {
            Ok(review) => status.review = Some(review),
            Err(error) => status.error = Some(format!("{error:#}").chars().take(2048).collect()),
        }
    }
}

fn review(
    locations: &Locations,
    runtime: &RuntimeReferences,
    directory: &str,
    managed: bool,
) -> anyhow::Result<CatalogCleanupReview> {
    let _exclusive = runtime.exclusive_review()?;
    let parent = locations
        .config
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let contents = if managed {
        lianli_control::media_ownership::review(parent, directory)?
    } else {
        catalog::review_catalog_directory(parent, directory)?
    };
    let mut references = [CatalogStorageEntry {
        directory: directory.into(),
        bytes: contents.bytes,
        ownership_verified: true,
        ..Default::default()
    }];
    crate::catalog_references::inspect(locations, &mut references)?;
    runtime.inspect(&mut references)?;
    let [references] = references;
    Ok(CatalogCleanupReview {
        contents,
        references,
    })
}

pub fn start(state: &SharedState, directory: String, managed: bool) -> IpcResponse {
    if directory.is_empty() || directory.len() > 160 || directory.contains(['/', '\\', '\0']) {
        return IpcResponse::error("Select a single media directory name");
    }
    let (job, instance, locations, runtime) = {
        let state = state.lock();
        (
            if managed {
                state.managed_review.clone()
            } else {
                state.catalog_review.clone()
            },
            state.info.instance_id.clone(),
            Locations {
                config: state.config_path.clone(),
                templates: state.templates_path(),
                presets: state.presets_path.clone(),
            },
            state.catalog_runtime.clone(),
        )
    };
    let initial = match reserve(&job, &instance) {
        Ok(status) => status,
        Err(error) => return error,
    };
    let worker_job = job.clone();
    // A blocked filesystem retains this single job without holding daemon state or delaying teardown.
    if let Err(error) = std::thread::Builder::new()
        .name("media-review".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                review(&locations, &runtime, &directory, managed)
            }))
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Media content review worker stopped unexpectedly"
                ))
            });
            finish(&worker_job, result);
        })
    {
        finish(
            &job,
            Err(anyhow::anyhow!("Could not start media review: {error}")),
        );
        return IpcResponse::error(format!("Could not start media review: {error}"));
    }
    IpcResponse::ok(&initial)
}

pub fn status(state: &SharedState, operation_id: &str, managed: bool) -> IpcResponse {
    let job = {
        let state = state.lock();
        if managed {
            state.managed_review.clone()
        } else {
            state.catalog_review.clone()
        }
    };
    let snapshot = job.lock().clone();
    match snapshot {
        Some(status) if status.operation_id == operation_id => IpcResponse::ok(&status),
        _ => IpcResponse::error(
            "Media review was replaced or the daemon restarted. Review the directory again",
        ),
    }
}

fn remove_reviewed(
    locations: &Locations,
    runtime: &RuntimeReferences,
    review: &CatalogCleanupReview,
    managed: bool,
) -> anyhow::Result<()> {
    let _exclusive = runtime.exclusive_review()?;
    let parent = locations
        .config
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let authorize = || {
        let mut references = [CatalogStorageEntry {
            directory: review.contents.directory.clone(),
            ..Default::default()
        }];
        crate::catalog_references::inspect(locations, &mut references)?;
        runtime.inspect(&mut references)?;
        let [references] = references;
        anyhow::ensure!(
            references.saved_references_checked
                && references.runtime_references_checked
                && references.saved_reference_count == 0
                && !references.runtime_referenced,
            "Media assets are referenced. Preserve them and review again after removing references"
        );
        Ok(())
    };
    if managed {
        lianli_control::media_removal::remove(
            parent,
            &review.contents.directory,
            &review.contents.sha256,
            authorize,
        )
    } else {
        catalog::remove_catalog_directory(
            parent,
            &review.contents.directory,
            &review.contents.sha256,
            authorize,
        )
    }
}

fn consume_review(
    job: &Job,
    operation_id: &str,
) -> anyhow::Result<(CatalogReviewStatus, CatalogCleanupReview)> {
    let mut slot = job.lock();
    let status = slot
        .as_mut()
        .filter(|status| status.operation_id == operation_id && status.finished)
        .ok_or_else(|| {
            anyhow::anyhow!("Media review is running, replaced or missing. Review again")
        })?;
    let review = status
        .review
        .take()
        .ok_or_else(|| anyhow::anyhow!("Review media files again before removal"))?;
    status.finished = false;
    status.error = None;
    status.removed = false;
    Ok((status.clone(), review))
}

pub fn remove(
    state: &SharedState,
    operation_id: &str,
    tx: super::EventSender,
    managed: bool,
) -> IpcResponse {
    let (job, locations, runtime) = {
        let state = state.lock();
        (
            if managed {
                state.managed_review.clone()
            } else {
                state.catalog_review.clone()
            },
            Locations {
                config: state.config_path.clone(),
                templates: state.templates_path(),
                presets: state.presets_path.clone(),
            },
            state.catalog_runtime.clone(),
        )
    };
    let (initial, review) = match consume_review(&job, operation_id) {
        Ok(review) => review,
        Err(error) => return IpcResponse::error(error.to_string()),
    };
    let worker_job = job.clone();
    // Retain the exclusive write permit until filesystem work ends, even after IPC disconnects.
    if let Err(error) = std::thread::Builder::new()
        .name("media-remove".into())
        .spawn(move || {
            let _permit = tx;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                remove_reviewed(&locations, &runtime, &review, managed)
            }))
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Media removal worker stopped unexpectedly. Inspect remaining files"
                ))
            });
            let mut slot = worker_job.lock();
            if let Some(status) = slot.as_mut() {
                status.finished = true;
                status.removed = result.is_ok();
                status.error = result
                    .err()
                    .map(|error| format!("{error:#}").chars().take(2048).collect());
            }
        })
    {
        finish(
            &job,
            Err(anyhow::anyhow!("Could not start media removal: {error}")),
        );
        return IpcResponse::error(format!("Could not start media removal: {error}"));
    }
    IpcResponse::ok(&initial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_and_catalog_reviews_have_separate_operation_slots() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(super::super::DaemonState::new(
            root.path().join("config.json"),
        )));
        let catalog = reserve(&state.lock().catalog_review, "instance").unwrap();
        let managed = reserve(&state.lock().managed_review, "instance").unwrap();
        assert!(matches!(
            status(&state, &catalog.operation_id, false),
            IpcResponse::Ok { .. }
        ));
        assert!(matches!(
            status(&state, &catalog.operation_id, true),
            IpcResponse::Error { .. }
        ));
        assert!(matches!(
            status(&state, &managed.operation_id, true),
            IpcResponse::Ok { .. }
        ));
        assert!(matches!(
            status(&state, &managed.operation_id, false),
            IpcResponse::Error { .. }
        ));
    }

    #[test]
    fn managed_orphan_removal_rechecks_saved_and_runtime_references() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let id = "1234567890abcdef1234567890abcdef";
        let receipts = root.path().join("media/receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        let receipt = receipts.join(format!("{id}.json"));
        std::fs::write(
            &receipt,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "id": id, "imports_inode": 1, "directory_inode": 2,
                "fingerprint": "a".repeat(64)
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
        let locations = Locations {
            config: root.path().join("config.json"),
            templates: root.path().join("lcd_templates.json"),
            presets: root.path().join("rgb_presets.json"),
        };
        let first = review(&locations, &RuntimeReferences::default(), id, true).unwrap();
        assert!(first.contents.files.is_empty());
        let asset = root
            .path()
            .join("media/imports")
            .join(id)
            .join("missing.png");
        std::fs::write(
            &locations.config,
            serde_json::to_vec(&serde_json::json!({
                "lcds": [{"index": 0, "type": "image", "path": asset}]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(remove_reviewed(&locations, &RuntimeReferences::default(), &first, true).is_err());
        assert!(receipt.exists());
        std::fs::remove_file(&locations.config).unwrap();
        let runtime = RuntimeReferences::default();
        let usage = runtime.enter(|| Ok(())).unwrap();
        usage.record(&[lianli_shared::media_dependencies::AssetDependency {
            path: asset,
            owner: "orphan fixture".into(),
            kind: lianli_shared::media_dependencies::AssetKind::Image,
        }]);
        drop(usage);
        assert!(remove_reviewed(&locations, &runtime, &first, true).is_err());
        assert!(receipt.exists());
        remove_reviewed(&locations, &RuntimeReferences::default(), &first, true).unwrap();
        assert!(!receipt.exists());
    }

    #[test]
    fn content_review_refreshes_saved_and_runtime_protection_without_mutation() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let name = "catalog-fixture-ABC123";
        let directory = root.path().join("templates").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = std::fs::metadata(&directory).unwrap();
        let asset = directory.join("asset.png");
        std::fs::write(&asset, b"x").unwrap();
        std::fs::set_permissions(&asset, std::fs::Permissions::from_mode(0o644)).unwrap();
        let receipt = directory.join(".lianli-catalog.json");
        std::fs::write(&receipt, serde_json::to_vec(&serde_json::json!({"schema_version":1,"directory_device":metadata.dev(),"directory_inode":metadata.ino(),"template_id":"fixture","files":[{"path":"asset.png","sha256":format!("{:x}",Sha256::digest(b"x"))}]})).unwrap()).unwrap();
        std::fs::set_permissions(receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
        let locations = Locations {
            config: root.path().join("config.json"),
            templates: root.path().join("lcd_templates.json"),
            presets: root.path().join("rgb_presets.json"),
        };
        let runtime = RuntimeReferences::default();
        let first = review(&locations, &runtime, name, false).unwrap();
        assert_eq!(first.references.saved_reference_count, 0);
        assert!(!first.references.runtime_referenced);
        let job = Job::default();
        let initial = reserve(&job, "fixture").unwrap();
        finish(&job, Ok(first.clone()));
        assert!(consume_review(&job, "wrong-instance").is_err());
        let (_, consumed) = consume_review(&job, &initial.operation_id).unwrap();
        assert_eq!(consumed.contents.sha256, first.contents.sha256);
        assert!(consume_review(&job, &initial.operation_id).is_err());
        std::fs::write(
            &locations.config,
            serde_json::to_vec(
                &serde_json::json!({"lcds":[{"index":0,"type":"image","path":asset}]}),
            )
            .unwrap(),
        )
        .unwrap();
        let usage = runtime.enter(|| Ok(())).unwrap();
        usage.record(&[lianli_shared::media_dependencies::AssetDependency {
            path: asset.clone(),
            owner: "fixture".into(),
            kind: lianli_shared::media_dependencies::AssetKind::Image,
        }]);
        drop(usage);
        let second = review(&locations, &runtime, name, false).unwrap();
        assert_eq!(first.contents.sha256, second.contents.sha256);
        assert_eq!(second.references.saved_reference_count, 1);
        assert!(second.references.runtime_referenced);
        assert!(remove_reviewed(&locations, &RuntimeReferences::default(), &first, false).is_err());
        std::fs::remove_file(&locations.config).unwrap();
        assert!(remove_reviewed(&locations, &runtime, &first, false).is_err());
        assert_eq!(std::fs::read(&asset).unwrap(), b"x");
        remove_reviewed(&locations, &RuntimeReferences::default(), &first, false).unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn review_admission_is_single_flight_and_keeps_bounded_terminal_errors() {
        let job = Job::default();
        let first = reserve(&job, "daemon-a").unwrap();
        assert!(reserve(&job, "daemon-a").is_err());
        finish(&job, Err(anyhow::anyhow!("x".repeat(4096))));
        assert!(job.lock().as_ref().unwrap().finished);
        assert_eq!(
            job.lock().as_ref().unwrap().error.as_ref().unwrap().len(),
            2048
        );
        let second = reserve(&job, "daemon-a").unwrap();
        assert_ne!(first.operation_id, second.operation_id);
        assert!(job.lock().as_ref().unwrap().error.is_none());
        assert!(lianli_shared::ipc::IpcRequest::StartCatalogReview {
            directory: "fixture".into()
        }
        .is_read_only());
    }
}
