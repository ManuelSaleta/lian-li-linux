use super::SharedState;
use crate::state_backups::{self, Locations};
use lianli_shared::backups::BackupTarget;
use lianli_shared::ipc::IpcResponse;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(false);
struct Running;
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

pub enum Operation {
    List,
    Preview {
        target: BackupTarget,
        preserved: bool,
    },
    Restore {
        target: BackupTarget,
        sha256: String,
    },
    Delete {
        target: BackupTarget,
        preserved: bool,
        sha256: String,
    },
}

pub fn run(state: &SharedState, operation: Operation, tx: super::EventSender) -> IpcResponse {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return IpcResponse::error("A backup operation is already running. Try again shortly.");
    }
    let running = Running;
    let locations = {
        let state = state.lock();
        Locations {
            config: state.config_path.clone(),
            templates: state.templates_path(),
            presets: state.presets_path.clone(),
        }
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = std::thread::Builder::new()
        .name("state-backup".into())
        .spawn(move || {
            let _running = running;
            let result = match operation {
                Operation::Restore { target, sha256 } => state_backups::restore(&locations, target, &sha256)
                    .and_then(|()| {
                        tx.send(crate::service::DaemonEvent::IpcUpdate).map_err(|_| anyhow::anyhow!("Backup was restored on disk, but the daemon stopped before reload. Reconnect and review settings"))?;
                        Ok(IpcResponse::ok(serde_json::json!(null)))
                    }),
                Operation::Preview { target, preserved } => {
                    let result = if preserved { state_backups::preview_record(&locations, target, true) }
                        else { state_backups::preview(&locations, target) };
                    result.map(|preview| IpcResponse::ok(&preview))
                }
                Operation::Delete { target, preserved, sha256 } => state_backups::delete(&locations, target, preserved, &sha256)
                    .map(|()| IpcResponse::ok(serde_json::json!(null))),
                Operation::List => state_backups::list(&locations).map(|entries| IpcResponse::ok(&entries)),
            };
            drop(tx);
            let _ = sender
                .send(result.unwrap_or_else(|error| IpcResponse::error(format!("{error:#}"))));
        })
    {
        return IpcResponse::error(format!("Could not start backup worker: {error}"));
    }
    // A stalled operation retains its worker and write permit after the IPC deadline.
    receiver.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| IpcResponse::error("Backup operation did not finish within three seconds and may still complete. Check state storage and reconnect to review settings before retrying"))
}
