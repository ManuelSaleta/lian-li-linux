use crate::account::Account;
use crate::reservation::{HardwareReservation, InheritedReservations, ServiceOperationLock};
use crate::state_transfer::PreparedTransfer;
use crate::transfer_channel::Channel;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REQUEST: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
pub enum Action {
    Publish(Box<PreparedTransfer>),
    Restore {
        config: PathBuf,
        backup: String,
    },
    ResumeRestore {
        config: PathBuf,
        backup: String,
        undo: String,
    },
    Recover {
        config: PathBuf,
    },
}

impl Action {
    fn config(&self) -> &std::path::Path {
        match self {
            Self::Publish(prepared) => &prepared.config_path,
            Self::Restore { config, .. }
            | Self::ResumeRestore { config, .. }
            | Self::Recover { config } => config,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    uid: u32,
    action: Action,
}

#[derive(Serialize, Deserialize)]
enum Message {
    OperationLock,
    HardwareLock,
    Request { bytes: usize, digest: String },
    Ready { backup: Option<String> },
    Proceed,
    Done { backup: Option<String> },
}

/// The caller keeps both reservations until the helper has exited. `record_ready`
/// must durably record the backup in the switch journal before authorizing mutation.
pub fn execute(
    account: &Account,
    action: Action,
    operation: &ServiceOperationLock,
    hardware: &HardwareReservation,
    record_ready: impl FnOnce(Option<&str>) -> Result<()>,
) -> Result<Option<String>> {
    if let Some(execution) = &account.container {
        execution.destination.check_config(action.config())?;
    }
    ensure!(
        std::env::current_exe()?.file_name() == Some(OsStr::new("lianli-control")),
        "Use the standalone control helper for account publication"
    );
    if let Action::Publish(prepared) = &action {
        ensure!(
            prepared.destination_uid == account.uid,
            "Prepared transfer belongs to another account"
        );
    }
    let command = account.control_command(&[OsStr::new("publish-state")])?;
    let result = run_command(
        command,
        operation.publication_descriptor()?,
        hardware.publication_descriptor()?,
        Request {
            uid: account.uid,
            action,
        },
        record_ready,
    );
    operation.verify()?;
    hardware.verify()?;
    result
}

fn run_command(
    command: Command,
    operation: BorrowedFd<'_>,
    hardware: BorrowedFd<'_>,
    request: Request,
    record_ready: impl FnOnce(Option<&str>) -> Result<()>,
) -> Result<Option<String>> {
    let needs_backup = !matches!(&request.action, Action::Recover { .. });
    let bytes = serde_json::to_vec(&request)?;
    ensure!(
        bytes.len() <= MAX_REQUEST,
        "Publication request exceeds 64 KiB"
    );
    let input = crate::state_transfer::sealed_state(&bytes)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let (parent, child) = Channel::pair()?;
    let channel = Channel::new(parent, TIMEOUT)?;
    std::thread::scope(|scope| {
        let worker = scope
            .spawn(move || crate::command::run_with_stdin(command, Stdio::from(child), TIMEOUT));
        let protocol = (|| {
            channel.send(&Message::OperationLock, Some(operation))?;
            channel.send(&Message::HardwareLock, Some(hardware))?;
            channel.send(
                &Message::Request {
                    bytes: bytes.len(),
                    digest,
                },
                Some(input.as_fd()),
            )?;
            let Message::Ready { backup } = plain(&channel)? else {
                anyhow::bail!("Publication helper did not reach its journal barrier")
            };
            ensure!(
                backup.is_some() == needs_backup,
                "Publication helper returned an unexpected backup state"
            );
            if let Some(backup) = &backup {
                crate::state_transaction::validate_backup(backup)?;
            }
            record_ready(backup.as_deref())?;
            channel.send(&Message::Proceed, None)?;
            let Message::Done { backup: completed } = plain(&channel)? else {
                anyhow::bail!("Publication helper did not confirm completion")
            };
            if let Some(completed) = &completed {
                crate::state_transaction::validate_backup(completed)?;
            }
            if backup.is_some() {
                ensure!(
                    backup == completed,
                    "Publication backup changed after authorization"
                );
            }
            Ok(completed)
        })();
        drop(channel);
        let output = worker
            .join()
            .map_err(|_| anyhow::anyhow!("Publication supervisor panicked"))?;
        let helper = output.and_then(|output| {
            ensure!(
                output.status.success(),
                "Account publication failed ({}): {}",
                output.status,
                output.stderr.chars().take(2048).collect::<String>()
            );
            ensure!(
                output.stdout.is_empty(),
                "Unexpected publication helper output"
            );
            Ok(())
        });
        match (protocol, helper) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(first), Err(second)) => anyhow::bail!("{first:#}. {second:#}"),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    })
}

pub fn serve() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Publish state under its unprivileged destination account"
    );
    let channel = Channel::new(std::io::stdin().as_fd().try_clone_to_owned()?, TIMEOUT)?;
    serve_channel(channel, InheritedReservations::new)
}

fn serve_channel(
    channel: Channel,
    inherit: impl FnOnce(File, File) -> Result<InheritedReservations>,
) -> Result<()> {
    let (message, operation): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::OperationLock),
        "Missing operation reservation"
    );
    let operation = File::from(operation.context("Missing operation descriptor")?);
    let (message, hardware): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::HardwareLock),
        "Missing hardware reservation"
    );
    let reservations = inherit(
        operation,
        File::from(hardware.context("Missing hardware descriptor")?),
    )?;
    let (message, input): (Message, _) = channel.receive()?;
    let Message::Request { bytes, digest } = message else {
        anyhow::bail!("Missing publication request")
    };
    ensure!(bytes <= MAX_REQUEST, "Publication request exceeds 64 KiB");
    let request: Request = serde_json::from_slice(&crate::state_transfer::read_sealed_state(
        input.context("Missing sealed publication request")?,
        bytes,
        &digest,
    )?)?;
    ensure!(
        request.uid == unsafe { libc::geteuid() },
        "Publication account changed"
    );
    reservations.verify()?;
    crate::container_destination::verify_config(request.action.config())?;
    let transaction = match &request.action {
        Action::Publish(prepared) => Some(crate::state_transfer::begin_publication(prepared)?),
        Action::Restore { config, backup } => {
            Some(crate::state_transaction::prepare_restore(config, backup)?)
        }
        Action::ResumeRestore {
            config,
            backup,
            undo,
        } => Some(crate::state_transaction::resume_restore(
            config, backup, undo,
        )?),
        Action::Recover { .. } => None,
    };
    channel.send(
        &Message::Ready {
            backup: transaction
                .as_ref()
                .map(|transaction| transaction.backup_name().to_string()),
        },
        None,
    )?;
    ensure!(
        matches!(plain(&channel)?, Message::Proceed),
        "Publication was not authorized after journaling"
    );
    reservations.verify()?;
    let backup = match (transaction, request.action) {
        (Some(transaction), Action::ResumeRestore { .. }) => {
            Some(transaction.resume_publication()?)
        }
        (Some(transaction), _) => Some(transaction.publish()?),
        (None, Action::Recover { config }) => crate::state_transaction::recover(&config)?,
        _ => unreachable!(),
    };
    reservations.verify()?;
    channel.send(&Message::Done { backup }, None)?;
    Ok(())
}

fn plain(channel: &Channel) -> Result<Message> {
    let (message, descriptor) = channel.receive()?;
    ensure!(
        descriptor.is_none(),
        "Unexpected descriptor in publication acknowledgement"
    );
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;

    #[test]
    fn worker_fixture() {
        let Some(root) = std::env::var_os("LIANLI_PUBLICATION_FIXTURE_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let paths = [root.join("operation"), root.join("hardware")];
        let channel = Channel::new(
            std::io::stdin().as_fd().try_clone_to_owned().unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        serve_channel(channel, |operation, hardware| {
            InheritedReservations::at(operation, hardware, paths, false)
        })
        .unwrap();
    }

    fn command(root: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "account_publication::tests::worker_fixture",
                "--nocapture",
            ])
            .env("LIANLI_PUBLICATION_FIXTURE_ROOT", root);
        unsafe {
            command.pre_exec(|| {
                if libc::dup2(2, 1) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }

    fn locks(root: &Path) -> (File, File) {
        let open = |name| {
            let path = root.join(name);
            fs::write(&path, "pid text").unwrap();
            let file = File::open(path).unwrap();
            assert_eq!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            file
        };
        (open("operation"), open("hardware"))
    }

    fn assert_busy(root: &Path) {
        for name in ["operation", "hardware"] {
            let file = File::open(root.join(name)).unwrap();
            assert_ne!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }
    }

    fn saved_backup(root: &Path) -> (PathBuf, String) {
        let config = root.join("config.json");
        fs::write(&config, b"{\"previous\":true}").unwrap();
        let backup = crate::state_transaction::StateTransaction::prepare(
            &config,
            &[crate::state::StateFile {
                relative_path: "config.json".into(),
                bytes: b"{\"current\":true}".to_vec(),
            }],
        )
        .unwrap()
        .publish()
        .unwrap();
        (config, backup)
    }

    #[test]
    fn account_worker_publishes_a_prepared_transfer_and_retains_parent_reservations() {
        let (source, target, prepared) = crate::state_transfer::prepared_fixture();
        let config = prepared.config_path.clone();
        let imports = prepared.final_media_path.clone();
        let original = fs::read(source.path().join("config.json")).unwrap();
        let (operation, hardware) = locks(target.path());
        let result = run_command(
            command(target.path()),
            operation.as_fd(),
            hardware.as_fd(),
            Request {
                uid: unsafe { libc::geteuid() },
                action: Action::Publish(Box::new(prepared)),
            },
            |backup| {
                assert!(backup.is_some());
                assert_eq!(fs::read(&config).unwrap(), b"{\"previous\":true}");
                assert!(!imports.exists());
                assert_busy(target.path());
                Ok(())
            },
        )
        .unwrap();
        assert!(result.is_some());
        let config: serde_json::Value = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
        assert_eq!(
            fs::read(config["lcds"][0]["path"].as_str().unwrap()).unwrap(),
            b"fixture media"
        );
        assert_eq!(
            fs::read(source.path().join("config.json")).unwrap(),
            original
        );
        assert_busy(target.path());
    }

    #[test]
    fn account_worker_restores_only_after_the_coordinator_records_its_backup() {
        let root = tempfile::tempdir().unwrap();
        let (operation, hardware) = locks(root.path());
        let (config, backup) = saved_backup(root.path());
        let mut recorded = None;
        let result = run_command(
            command(root.path()),
            operation.as_fd(),
            hardware.as_fd(),
            Request {
                uid: unsafe { libc::geteuid() },
                action: Action::Restore {
                    config: config.clone(),
                    backup: backup.clone(),
                },
            },
            |backup| {
                assert_eq!(fs::read(&config).unwrap(), b"{\"current\":true}");
                assert_busy(root.path());
                let backup = backup.unwrap();
                assert!(root.path().join(backup).join("manifest.json").is_file());
                recorded = Some(backup.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result, recorded);
        assert_eq!(fs::read(&config).unwrap(), b"{\"previous\":true}");
        assert_busy(root.path());
        let repeated = run_command(
            command(root.path()),
            operation.as_fd(),
            hardware.as_fd(),
            Request {
                uid: unsafe { libc::geteuid() },
                action: Action::ResumeRestore {
                    config: config.clone(),
                    backup,
                    undo: result.clone().unwrap(),
                },
            },
            |backup| {
                assert_eq!(backup, result.as_deref());
                assert_busy(root.path());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(repeated, result);
        crate::state_transaction::restore_backup(&config, result.as_deref().unwrap()).unwrap();
        assert_eq!(fs::read(config).unwrap(), b"{\"current\":true}");
    }

    #[test]
    fn failed_journal_barrier_leaves_state_unchanged_and_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let (operation, hardware) = locks(root.path());
        let (config, backup) = saved_backup(root.path());
        let error = run_command(
            command(root.path()),
            operation.as_fd(),
            hardware.as_fd(),
            Request {
                uid: unsafe { libc::geteuid() },
                action: Action::Restore {
                    config: config.clone(),
                    backup,
                },
            },
            |_| anyhow::bail!("fixture journal storage failed"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("journal storage failed"));
        assert_eq!(fs::read(&config).unwrap(), b"{\"current\":true}");
        assert!(root.path().join(".lianli-state-transaction.json").is_file());
        assert_busy(root.path());
        let result = run_command(
            command(root.path()),
            operation.as_fd(),
            hardware.as_fd(),
            Request {
                uid: unsafe { libc::geteuid() },
                action: Action::Recover {
                    config: config.clone(),
                },
            },
            |backup| {
                assert!(backup.is_none());
                Ok(())
            },
        )
        .unwrap();
        assert!(result.is_some());
        assert!(!root.path().join(".lianli-state-transaction.json").exists());
        assert_eq!(fs::read(&config).unwrap(), b"{\"current\":true}");
        assert_busy(root.path());
    }

    #[test]
    fn replaced_reservation_after_the_barrier_prevents_settings_mutation() {
        let root = tempfile::tempdir().unwrap();
        let (operation, hardware) = locks(root.path());
        let (config, backup) = saved_backup(root.path());
        let result = run_command(
            command(root.path()),
            operation.as_fd(),
            hardware.as_fd(),
            Request {
                uid: unsafe { libc::geteuid() },
                action: Action::Restore {
                    config: config.clone(),
                    backup,
                },
            },
            |_| {
                fs::remove_file(root.path().join("hardware"))?;
                fs::write(root.path().join("hardware"), "replacement")?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&config).unwrap(), b"{\"current\":true}");
        crate::state_transaction::recover(&config).unwrap();
    }
}
