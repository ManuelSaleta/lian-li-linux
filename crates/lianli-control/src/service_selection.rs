use crate::reservation::{HardwareReservation, ServiceOperationLock};
use crate::services::Route;
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceScope, ServiceSelection};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const DIRECTORY: &str = "/etc/lianli";
const FILE: &str = "service-selection.json";
const MAX_BYTES: usize = 4096;

pub fn select(scope: ServiceScope, user_uid: Option<u32>) -> Result<ServiceSelection> {
    require_administrator()?;
    let uid = account_uid(scope, user_uid)?;
    let context = InstallationContext::Native;
    let operation = ServiceOperationLock::acquire(&context)?;
    let ownership = crate::ownership::inspect(&context, &Route::Native)?;
    ensure!(
        ownership.owner_pid.is_none(),
        "Stop the current daemon cleanly before changing host service selection"
    );
    let hardware = HardwareReservation::acquire(&context, &ownership.identity)?;
    let selection = ServiceSelection { scope, uid };
    replace(Some(selection), &operation, &hardware)?;
    Ok(selection)
}

fn require_administrator() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0
            && InstallationContext::detect() == InstallationContext::Native,
        "Only a native, authorized administrator can change host service selection"
    );
    Ok(())
}

fn account_uid(scope: ServiceScope, user_uid: Option<u32>) -> Result<u32> {
    ensure!(
        (scope == ServiceScope::User) == user_uid.is_some(),
        "User mode requires --user-uid. System mode always uses the packaged lianli account"
    );
    let mut buffer = vec![0u8; 16 * 1024];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let result = unsafe {
        if let Some(uid) = user_uid {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        } else {
            libc::getpwnam_r(
                c"lianli".as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        }
    };
    ensure!(result == 0 && !found.is_null(), "The selected daemon account is unavailable. Check the installed sysusers rule or user account");
    let uid = unsafe { entry.assume_init().pw_uid };
    ensure!(
        uid != 0,
        "Hardware service selection requires an unprivileged account"
    );
    Ok(uid)
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    version: u32,
    selection: Option<ServiceSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
}

impl Record {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.selection.is_none_or(|selection| selection.uid != 0),
            "Host service selection requires an unprivileged daemon account"
        );
        match (self.version, &self.operation) {
            (1, None) => ensure!(self.selection.is_some(), "Missing host service selection"),
            (2, Some(id)) => {
                lianli_shared::daemon::parse_service_invocation(id).map_err(anyhow::Error::msg)?;
            }
            _ => anyhow::bail!("Unsupported host service selection version or startup gate"),
        }
        Ok(())
    }

    fn allows(&self, scope: ServiceScope, uid: u32) -> bool {
        self.operation.is_none() && self.selection.is_none_or(|value| value.allows(scope, uid))
    }

    fn blocked_reason(&self, scope: ServiceScope, uid: u32) -> Option<String> {
        if self.operation.is_some() {
            return Some("Daemon startup is paused for an unfinished service switch. Use Recover interrupted switch in the host GUI.".into());
        }
        self.selection.filter(|selected| !selected.allows(scope, uid)).map(|selected| {
            let selected_mode = match selected.scope { ServiceScope::User => "user", ServiceScope::System => "system" };
            let requested_mode = match scope { ServiceScope::User => "user", ServiceScope::System => "system" };
            format!("Host selection is {selected_mode} mode for UID {}. This launch requests {requested_mode} mode as UID {uid} and will remain inactive. Change the selection through host service controls before launching another mode or account.", selected.uid)
        })
    }
}

pub fn inspect(context: &InstallationContext) -> Result<Option<ServiceSelection>> {
    let record = inspect_record(context)?;
    ensure!(record.as_ref().is_none_or(|record| record.operation.is_none()),
        "Daemon startup is paused for an unfinished service switch. Recover the switch before starting either mode");
    Ok(record.and_then(|record| record.selection))
}

fn inspect_record(context: &InstallationContext) -> Result<Option<Record>> {
    let (path, root_owned) = match context {
        InstallationContext::Native => (PathBuf::from(DIRECTORY), true),
        InstallationContext::Distrobox { .. } => {
            (PathBuf::from(format!("/run/host{DIRECTORY}")), false)
        }
        InstallationContext::UnsupportedContainer => {
            anyhow::bail!("Host service selection is unavailable in this container")
        }
    };
    let directory = open_directory(&path, root_owned)?;
    if !root_owned {
        let route = Route::detect(context)?;
        verify_host_entry(&route, DIRECTORY, directory.as_ref(), libc::S_IFDIR)?;
        if let Some(directory) = &directory {
            let file = open_file(directory)?;
            verify_host_entry(
                &route,
                &format!("{DIRECTORY}/{FILE}"),
                file.as_ref(),
                libc::S_IFREG,
            )?;
            return file.map(parse).transpose();
        }
    }
    directory
        .as_ref()
        .map(read_record)
        .transpose()
        .map(Option::flatten)
}

pub fn launch_allowed(context: &InstallationContext, scope: ServiceScope) -> Result<bool> {
    Ok(inspect_record(context)?
        .is_none_or(|record| record.allows(scope, unsafe { libc::geteuid() })))
}

pub fn launch_block_reason(
    context: &InstallationContext,
    scope: ServiceScope,
) -> Result<Option<String>> {
    Ok(inspect_record(context)?
        .and_then(|record| record.blocked_reason(scope, unsafe { libc::geteuid() })))
}

/// Resolve destination accounts before reserving hardware; NSS lookups can block.
pub fn replace(
    selection: Option<ServiceSelection>,
    operation: &ServiceOperationLock,
    hardware: &HardwareReservation,
) -> Result<Option<ServiceSelection>> {
    require_administrator()?;
    if let Some(selection) = selection {
        ensure!(
            selection.uid != 0,
            "Hardware service selection requires an unprivileged account"
        );
    }
    operation.verify()?;
    hardware.verify()?;
    let directory = writable_directory()?;
    let previous = read_record(&directory)?;
    ensure!(
        previous
            .as_ref()
            .is_none_or(|record| record.operation.is_none()),
        "Recover the pending service switch before replacing host service selection"
    );
    verify_directory_path(&directory, Path::new(DIRECTORY))?;
    publish(&directory, selection)?;
    verify_directory_path(&directory, Path::new(DIRECTORY))?;
    Ok(previous.and_then(|record| record.selection))
}

pub(crate) fn pause(operation_id: &str, operation: &ServiceOperationLock) -> Result<()> {
    require_administrator()?;
    operation.verify()?;
    let directory = writable_directory()?;
    let record = paused(read_record(&directory)?, operation_id)?;
    verify_directory_path(&directory, Path::new(DIRECTORY))?;
    publish_record(&directory, &record)?;
    verify_directory_path(&directory, Path::new(DIRECTORY))
}

pub(crate) fn require_paused(operation_id: &str) -> Result<()> {
    require_administrator()?;
    let record = inspect_record(&InstallationContext::Native)?;
    ensure!(
        record
            .as_ref()
            .and_then(|record| record.operation.as_deref())
            == Some(operation_id),
        "Pause startup for this switch before changing service enablement"
    );
    Ok(())
}

pub(crate) fn resume(
    selection: Option<ServiceSelection>,
    operation_id: &str,
    operation: &ServiceOperationLock,
    hardware: &HardwareReservation,
) -> Result<()> {
    require_administrator()?;
    operation.verify()?;
    hardware.verify()?;
    let directory = writable_directory()?;
    verify_resume(read_record(&directory)?, operation_id, selection)?;
    verify_directory_path(&directory, Path::new(DIRECTORY))?;
    publish(&directory, selection)?;
    verify_directory_path(&directory, Path::new(DIRECTORY))
}

pub(crate) fn resume_live_system(
    selection: ServiceSelection,
    operation_id: &str,
    operation: &ServiceOperationLock,
    mut verify_source: impl FnMut() -> Result<()>,
) -> Result<()> {
    require_administrator()?;
    operation.verify()?;
    let directory = writable_directory()?;
    resume_live_at(
        &directory,
        Path::new(DIRECTORY),
        selection,
        operation_id,
        0,
        || {
            operation.verify()?;
            verify_source()
        },
    )
}

fn resume_live_at(
    directory: &File,
    path: &Path,
    selection: ServiceSelection,
    operation_id: &str,
    owner: u32,
    mut verify_source: impl FnMut() -> Result<()>,
) -> Result<()> {
    ensure!(
        selection.scope == ServiceScope::System && selection.uid != 0,
        "Live recovery can only reopen startup for its verified system source"
    );
    let previous = read_owned_record(directory, owner)?;
    verify_resume(
        read_owned_record(directory, owner)?,
        operation_id,
        Some(selection),
    )?;
    // The authenticated running source owns hardware; keep every other launch route blocked.
    verify_source()?;
    ensure!(
        read_owned_record(directory, owner)? == previous,
        "The startup gate changed during live source verification"
    );
    verify_directory_path(directory, path)?;
    if previous
        .as_ref()
        .is_some_and(|record| record.operation.is_some())
    {
        publish(directory, Some(selection))?;
    }
    verify_directory_path(directory, path)?;
    verify_source()
}

fn verify_directory_path(directory: &File, path: &Path) -> Result<()> {
    let held = directory.metadata()?;
    let current = fs::symlink_metadata(path)?;
    ensure!(
        current.is_dir() && (held.dev(), held.ino()) == (current.dev(), current.ino()),
        "The host service selection directory changed during publication"
    );
    Ok(())
}

fn verify_resume(
    current: Option<Record>,
    operation_id: &str,
    selection: Option<ServiceSelection>,
) -> Result<()> {
    lianli_shared::daemon::parse_service_invocation(operation_id).map_err(anyhow::Error::msg)?;
    if let Some(id) = current
        .as_ref()
        .and_then(|record| record.operation.as_deref())
    {
        ensure!(
            id == operation_id,
            "The startup gate belongs to a different service switch"
        );
    } else {
        ensure!(
            current.and_then(|record| record.selection) == selection,
            "The switch startup gate is missing and the service selection differs"
        );
    }
    Ok(())
}

fn paused(previous: Option<Record>, operation_id: &str) -> Result<Record> {
    lianli_shared::daemon::parse_service_invocation(operation_id).map_err(anyhow::Error::msg)?;
    ensure!(
        previous
            .as_ref()
            .and_then(|record| record.operation.as_deref())
            .is_none_or(|id| id == operation_id),
        "Another switch owns the startup gate"
    );
    Ok(Record {
        version: 2,
        selection: previous.and_then(|record| record.selection),
        operation: Some(operation_id.into()),
    })
}

fn writable_directory() -> Result<File> {
    let path = Path::new(DIRECTORY);
    let directory = match open_directory(path, true)? {
        Some(directory) => directory,
        None => {
            fs::create_dir(path).context("Creating the host service selection directory")?;
            let directory =
                open_directory(path, true)?.context("Service selection directory disappeared")?;
            directory.set_permissions(Permissions::from_mode(0o755))?;
            File::open("/etc")?.sync_all()?;
            directory
        }
    };
    ensure!(
        directory.metadata()?.mode() & 0o055 == 0o055,
        "Host service selection directory must be readable and searchable by daemon accounts"
    );
    Ok(directory)
}

fn open_directory(path: &Path, root_owned: bool) -> Result<Option<File>> {
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Opening host service selection directory"),
    };
    let metadata = directory.metadata()?;
    ensure!(
        (!root_owned || metadata.uid() == 0) && metadata.mode() & 0o022 == 0,
        "Host service selection directory must be root-owned and not writable by other accounts"
    );
    Ok(Some(directory))
}

fn open_file(directory: &File) -> Result<Option<File>> {
    let pinned = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}/{FILE}", directory.as_raw_fd()))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Opening host service selection"),
    };
    ensure!(
        pinned.metadata()?.is_file(),
        "Host service selection is not a regular file"
    );
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
        .map(Some)
        .context("Reading the pinned host service selection")
}

fn read_record(directory: &File) -> Result<Option<Record>> {
    read_owned_record(directory, 0)
}

fn read_owned_record(directory: &File, owner: u32) -> Result<Option<Record>> {
    let Some(file) = open_file(directory)? else {
        return Ok(None);
    };
    ensure!(
        file.metadata()?.uid() == owner,
        "Host service selection belongs to an unexpected account"
    );
    parse(file).map(Some)
}

fn parse(file: File) -> Result<Record> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.mode() & 0o022 == 0 && metadata.len() <= MAX_BYTES as u64,
        "Host service selection must be a protected regular file of at most 4096 bytes"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "Host service selection grew beyond its size limit"
    );
    let record: Record =
        serde_json::from_slice(&bytes).context("Invalid host service selection")?;
    record.validate()?;
    Ok(record)
}

fn publish(directory: &File, selection: Option<ServiceSelection>) -> Result<()> {
    let parent = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    if let Some(selection) = selection {
        return publish_record(
            directory,
            &Record {
                version: 1,
                selection: Some(selection),
                operation: None,
            },
        );
    } else {
        match fs::remove_file(parent.join(FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    directory.sync_all()?;
    Ok(())
}

fn publish_record(directory: &File, record: &Record) -> Result<()> {
    record.validate()?;
    let parent = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut file = tempfile::NamedTempFile::new_in(&parent)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.as_file()
        .set_permissions(Permissions::from_mode(0o644))?;
    file.as_file().sync_all()?;
    file.persist(parent.join(FILE))?;
    directory.sync_all()?;
    Ok(())
}

fn verify_host_entry(route: &Route, path: &str, local: Option<&File>, kind: u32) -> Result<()> {
    let output = route.output("/usr/bin/stat", &["--printf=%d %i %f %u\n", "--", path])?;
    if !output.status.success() {
        ensure!(
            local.is_none() && output.stderr.contains("No such file or directory"),
            "Cannot verify host service selection visibility: {}",
            output.stderr.trim()
        );
        return Ok(());
    }
    let file = local.context("The host service selection is hidden inside this container")?;
    verify_host_metadata(&output.stdout, &file.metadata()?, kind)
}

fn verify_host_metadata(text: &str, local: &fs::Metadata, kind: u32) -> Result<()> {
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure!(fields.len() == 4, "Invalid host service selection metadata");
    let mode = u32::from_str_radix(fields[2], 16)?;
    ensure!(
        mode & libc::S_IFMT == kind
            && mode & 0o022 == 0
            && fields[3] == "0"
            && fields[0].parse::<u64>()? == local.dev()
            && fields[1].parse::<u64>()? == local.ino(),
        "Host service selection differs from the container view or is not protected and root-owned"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn read_fixture(directory: &File) -> Result<Option<Record>> {
        read_owned_record(directory, unsafe { libc::geteuid() })
    }

    fn resume_fixture(
        directory: &File,
        path: &Path,
        selection: ServiceSelection,
        id: &str,
        verify: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        resume_live_at(
            directory,
            path,
            selection,
            id,
            unsafe { libc::geteuid() },
            verify,
        )
    }

    #[test]
    fn launch_rejection_explains_pauses_and_selected_accounts_without_changing_policy() {
        let selected = Record {
            version: 1,
            selection: Some(ServiceSelection {
                scope: ServiceScope::System,
                uid: 2000,
            }),
            operation: None,
        };
        let paused = paused(Some(selected), "123456789abcdef0123456789abcdef0").unwrap();
        let selected = Record {
            version: 1,
            selection: Some(ServiceSelection {
                scope: ServiceScope::System,
                uid: 2000,
            }),
            operation: None,
        };
        for record in [&selected, &paused] {
            for scope in [ServiceScope::User, ServiceScope::System] {
                for uid in [1000, 2000] {
                    assert_eq!(
                        record.blocked_reason(scope, uid).is_none(),
                        record.allows(scope, uid)
                    );
                }
            }
        }
        let mismatch = selected.blocked_reason(ServiceScope::User, 1000).unwrap();
        assert!(mismatch.contains("system mode for UID 2000"));
        assert!(mismatch.contains("user mode as UID 1000"));
        let wrong_uid = selected.blocked_reason(ServiceScope::System, 1000).unwrap();
        assert!(wrong_uid.contains("system mode as UID 1000"));
        let reason = paused.blocked_reason(ServiceScope::System, 2000).unwrap();
        assert!(reason.contains("unfinished service switch"));
        assert!(reason.contains("Recover interrupted switch"));
    }

    #[test]
    fn live_system_recovery_reopens_only_its_source_and_survives_interruption() {
        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path(), false).unwrap().unwrap();
        let id = "123456789abcdef0123456789abcdef0";
        let system = ServiceSelection {
            scope: ServiceScope::System,
            uid: 2000,
        };
        let paused = paused(None, id).unwrap();
        publish_record(&directory, &paused).unwrap();
        let mut calls = 0;
        assert!(resume_fixture(&directory, root.path(), system, id, || {
            calls += 1;
            if calls == 1 {
                assert!(!read_fixture(&directory)?
                    .unwrap()
                    .allows(ServiceScope::System, 2000));
                Ok(())
            } else {
                anyhow::bail!("Fixture process exited after publication")
            }
        })
        .is_err());
        assert_eq!(calls, 2);
        let selected = read_fixture(&directory).unwrap().unwrap();
        assert!(selected.allows(ServiceScope::System, 2000));
        assert!(!selected.allows(ServiceScope::System, 2001));
        assert!(!selected.allows(ServiceScope::User, 1000));
        let inode = open_file(&directory)
            .unwrap()
            .unwrap()
            .metadata()
            .unwrap()
            .ino();
        resume_fixture(&directory, root.path(), system, id, || Ok(())).unwrap();
        assert_eq!(
            open_file(&directory)
                .unwrap()
                .unwrap()
                .metadata()
                .unwrap()
                .ino(),
            inode
        );
    }

    #[test]
    fn live_recovery_preserves_unverified_foreign_or_replaced_gates() {
        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path(), false).unwrap().unwrap();
        let id = "123456789abcdef0123456789abcdef0";
        let other_id = "abcdef0123456789abcdef0123456789";
        let system = ServiceSelection {
            scope: ServiceScope::System,
            uid: 2000,
        };
        let gate = paused(None, id).unwrap();
        for target in [
            selection(),
            ServiceSelection {
                scope: ServiceScope::System,
                uid: 0,
            },
        ] {
            publish_record(&directory, &gate).unwrap();
            assert!(
                resume_fixture(&directory, root.path(), target, id, || panic!(
                    "Invalid mode must not reach the verifier"
                ))
                .is_err()
            );
            assert_eq!(
                read_fixture(&directory).unwrap(),
                Some(paused(None, id).unwrap())
            );
        }
        for original in [
            None,
            Some(selection()),
            Some(ServiceSelection {
                scope: ServiceScope::System,
                uid: 2001,
            }),
        ] {
            publish(&directory, original).unwrap();
            assert!(
                resume_fixture(&directory, root.path(), system, id, || panic!(
                    "Unrelated selection must not reach the verifier"
                ))
                .is_err()
            );
        }
        publish_record(&directory, &gate).unwrap();
        assert!(
            resume_fixture(&directory, root.path(), system, other_id, || panic!(
                "Foreign operation must not reach the verifier"
            ))
            .is_err()
        );
        assert!(
            resume_fixture(&directory, root.path(), system, id, || anyhow::bail!(
                "Fixture wrong source owner"
            ))
            .is_err()
        );
        assert_eq!(read_fixture(&directory).unwrap(), Some(gate));
        assert!(resume_fixture(&directory, root.path(), system, id, || {
            publish_record(&directory, &paused(None, other_id)?)
        })
        .is_err());
        assert_eq!(
            read_fixture(&directory).unwrap(),
            Some(paused(None, other_id).unwrap())
        );
    }

    fn selection() -> ServiceSelection {
        ServiceSelection {
            scope: ServiceScope::User,
            uid: 1000,
        }
    }

    #[test]
    fn publication_replaces_one_record_and_removal_restores_unselected_behavior() {
        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path(), false).unwrap().unwrap();
        assert!(open_file(&directory).unwrap().is_none());
        publish(&directory, Some(selection())).unwrap();
        let old = open_file(&directory).unwrap().unwrap();
        let other = ServiceSelection {
            scope: ServiceScope::System,
            uid: 999,
        };
        publish(&directory, Some(other)).unwrap();
        assert_eq!(parse(old).unwrap().selection, Some(selection()));
        assert_eq!(
            parse(open_file(&directory).unwrap().unwrap())
                .unwrap()
                .selection,
            Some(other)
        );
        assert_eq!(
            fs::metadata(root.path().join(FILE)).unwrap().mode() & 0o777,
            0o644
        );
        publish(&directory, None).unwrap();
        assert!(open_file(&directory).unwrap().is_none());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn paused_startup_survives_reopening_and_blocks_both_accounts() {
        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path(), false).unwrap().unwrap();
        let operation = "0123456789abcdef0123456789abcdef";
        for original in [None, Some(selection())] {
            publish(&directory, original).unwrap();
            let read = || {
                open_file(&directory)
                    .unwrap()
                    .map(parse)
                    .transpose()
                    .unwrap()
            };
            let record = paused(read(), operation).unwrap();
            publish_record(&directory, &record).unwrap();
            for scope in [ServiceScope::User, ServiceScope::System] {
                for uid in [0, 999, 1000, 1001] {
                    assert!(!read().unwrap().allows(scope, uid));
                }
            }
            assert_eq!(read().unwrap().selection, original);
            assert_eq!(read().unwrap().operation.as_deref(), Some(operation));
            assert!(paused(read(), &"f".repeat(32)).is_err());
            assert!(verify_resume(read(), &"f".repeat(32), original).is_err());
            publish_record(&directory, &paused(read(), operation).unwrap()).unwrap();
            verify_resume(read(), operation, original).unwrap();
            publish(&directory, original).unwrap();
            verify_resume(read(), operation, original).unwrap();
            assert_eq!(
                fs::read_dir(root.path()).unwrap().count(),
                usize::from(original.is_some())
            );
            if let Some(record) = read() {
                assert!(record.allows(ServiceScope::User, 1000));
                assert!(!record.allows(ServiceScope::System, 999));
                assert!(!record.allows(ServiceScope::User, 1001));
            }
        }
    }

    #[test]
    fn moved_or_replaced_selection_directories_abort_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selection");
        fs::create_dir(&path).unwrap();
        let directory = open_directory(&path, false).unwrap().unwrap();
        assert!(verify_directory_path(&directory, &path).is_ok());
        fs::rename(&path, root.path().join("moved")).unwrap();
        assert!(verify_directory_path(&directory, &path).is_err());
        fs::create_dir(&path).unwrap();
        assert!(verify_directory_path(&directory, &path).is_err());
        fs::remove_dir(&path).unwrap();
        symlink(root.path().join("moved"), &path).unwrap();
        assert!(verify_directory_path(&directory, &path).is_err());
    }

    #[test]
    fn resume_requires_the_matching_gate_or_already_published_selection() {
        let operation = "0123456789abcdef0123456789abcdef";
        let selected = || {
            Some(Record {
                version: 1,
                selection: Some(selection()),
                operation: None,
            })
        };
        assert!(verify_resume(None, operation, Some(selection())).is_err());
        assert!(verify_resume(selected(), operation, None).is_err());
        assert!(verify_resume(selected(), operation, Some(selection())).is_ok());
        assert!(verify_resume(None, operation, None).is_ok());
        assert!(verify_resume(None, "invalid", None).is_err());
        assert!(paused(selected(), "invalid").is_err());
    }

    #[test]
    fn paused_records_are_rejected_by_the_older_daemon_parser() {
        #[derive(Deserialize)]
        struct OldRecord {
            version: u32,
            selection: ServiceSelection,
        }
        for original in [
            None,
            Some(Record {
                version: 1,
                selection: Some(selection()),
                operation: None,
            }),
        ] {
            let paused = paused(original, &"a".repeat(32)).unwrap();
            let bytes = serde_json::to_vec(&paused).unwrap();
            if let Ok(old) = serde_json::from_slice::<OldRecord>(&bytes) {
                assert!(!(old.version == 1 && old.selection.uid != 0));
            }
        }
    }

    #[test]
    fn malformed_unsafe_and_oversized_selection_never_becomes_unselected() {
        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path(), false).unwrap().unwrap();
        for bytes in [
            b"broken".as_slice(),
            br#"{"version":2,"selection":{"scope":"user","uid":1000}}"#,
            br#"{"version":1,"selection":{"scope":"system","uid":0}}"#,
            br#"{"version":1,"selection":null}"#,
            br#"{"version":1,"selection":{"scope":"user","uid":1000},"operation":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
            br#"{"version":2,"selection":null,"operation":null}"#,
            br#"{"version":2,"selection":null,"operation":"invalid"}"#,
        ] {
            fs::write(root.path().join(FILE), bytes).unwrap();
            assert!(parse(open_file(&directory).unwrap().unwrap()).is_err());
        }
        fs::write(root.path().join(FILE), vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(parse(open_file(&directory).unwrap().unwrap()).is_err());
        publish(&directory, Some(selection())).unwrap();
        fs::set_permissions(root.path().join(FILE), Permissions::from_mode(0o666)).unwrap();
        assert!(parse(open_file(&directory).unwrap().unwrap()).is_err());
        fs::remove_file(root.path().join(FILE)).unwrap();
        fs::write(root.path().join("target"), b"{}").unwrap();
        symlink("target", root.path().join(FILE)).unwrap();
        assert!(open_file(&directory).is_err());
        fs::remove_file(root.path().join(FILE)).unwrap();
        fs::create_dir(root.path().join(FILE)).unwrap();
        assert!(open_file(&directory).is_err());
    }

    #[test]
    fn container_metadata_must_match_the_pinned_root_owned_host_object() {
        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path(), false).unwrap().unwrap();
        publish(&directory, Some(selection())).unwrap();
        let metadata = open_file(&directory).unwrap().unwrap().metadata().unwrap();
        let valid = format!(
            "{} {} {:x} 0\n",
            metadata.dev(),
            metadata.ino(),
            libc::S_IFREG | 0o644
        );
        assert!(verify_host_metadata(&valid, &metadata, libc::S_IFREG).is_ok());
        for invalid in [
            format!(
                "{} {} {:x} 1000",
                metadata.dev(),
                metadata.ino(),
                libc::S_IFREG | 0o644
            ),
            format!(
                "{} {} {:x} 0",
                metadata.dev(),
                metadata.ino() + 1,
                libc::S_IFREG | 0o644
            ),
            format!(
                "{} {} {:x} 0",
                metadata.dev(),
                metadata.ino(),
                libc::S_IFREG | 0o666
            ),
        ] {
            assert!(verify_host_metadata(&invalid, &metadata, libc::S_IFREG).is_err());
        }
        assert!(verify_host_metadata(&valid, &metadata, libc::S_IFDIR).is_err());
    }
}
