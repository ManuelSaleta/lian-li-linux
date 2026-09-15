use crate::services::Route;
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceActionRequest, ServiceOperationStatus};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const UNIT: &str = "lianli-control-operation.service";
const MAX_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobStatus {
    version: u32,
    #[serde(default)]
    pub updated_at_ms: u64,
    pub id: String,
    pub request: ServiceActionRequest,
    pub status: ServiceOperationStatus,
}

struct Directory(File);

impl Directory {
    fn open(path: &Path, create: bool) -> Result<Option<Self>> {
        if create {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("Creating service progress directory"),
            }
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None)
            }
            Err(error) => return Err(error).context("Opening service progress directory"),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Service progress directory must be private and belong to this account"
        );
        Ok(Some(Self(file)))
    }

    fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd())).join(name)
    }

    fn lock(&self, create: bool) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(create)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path("worker.lock"))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1,
            "Invalid service progress lock"
        );
        Ok(file)
    }

    fn read(&self) -> Result<Option<JobStatus>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path("status.json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("Reading service progress"),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.len() <= MAX_BYTES,
            "Invalid service progress file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Service progress exceeds its size limit"
        );
        let record: JobStatus = serde_json::from_slice(&bytes)?;
        ensure!(
            record.version == 1
                && valid_id(&record.id)
                && record.status.active == record.status.success.is_none(),
            "Unsupported or inconsistent service progress record"
        );
        Ok(Some(record))
    }

    fn write(&self, record: &JobStatus) -> Result<()> {
        let mut record = record.clone();
        record.updated_at_ms = crate::switch_job::timestamp();
        let bytes = serde_json::to_vec(&record)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Service progress exceeds its size limit"
        );
        let mut temporary = tempfile::NamedTempFile::new_in(self.path(""))?;
        temporary.write_all(&bytes)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.as_file().sync_all()?;
        temporary.persist(self.path("status.json"))?;
        self.0.sync_all()?;
        Ok(())
    }
}

struct WorkerLock(File);

impl Drop for WorkerLock {
    fn drop(&mut self) {
        // Explicit unlock also releases copies inherited by unrelated fork-to-exec children.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn try_lock(file: &File, operation: i32) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error).context("Checking service progress ownership")
}

fn runtime_path() -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    ensure!(
        uid != 0,
        "Service jobs run under the desktop user, not root"
    );
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let metadata = fs::symlink_metadata(&runtime)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "A private desktop-user runtime directory is required for service progress"
    );
    Ok(runtime.join("lianli-control"))
}

pub fn read() -> Result<Option<JobStatus>> {
    read_at(&runtime_path()?)
}

fn read_at(path: &Path) -> Result<Option<JobStatus>> {
    let Some(directory) = Directory::open(path, false)? else {
        return Ok(None);
    };
    let Some(mut record) = directory.read()? else {
        return Ok(None);
    };
    if record.status.active {
        let lock = directory
            .lock(false)
            .context("Service progress has no worker lock")?;
        if try_lock(&lock, libc::LOCK_SH)? {
            let _lock = WorkerLock(lock);
            // Completion may have been published between the first read and the lock attempt.
            record = directory.read()?.context("Service progress disappeared")?;
            if record.status.active {
                record.status = ServiceOperationStatus {
                    active: false,
                    success: Some(false),
                    message:
                        "Service verification was interrupted. Recheck services before retrying."
                            .into(),
                };
            }
        }
    }
    Ok(Some(record))
}

pub fn start(context: &InstallationContext, request: ServiceActionRequest) -> Result<String> {
    ensure!(
        !read()?.is_some_and(|record| record.status.active),
        "Another service operation is running"
    );
    let helper = std::env::current_exe()?
        .parent()
        .context("Application directory is unavailable")?
        .join("lianli-control");
    ensure!(fs::metadata(&helper).is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0),
        "Install or build the matching lianli-control executable beside the GUI before using service controls");
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let args = launch_arguments(context, request, &helper, &id)?;
    let route = Route::detect(context)?;
    let result = route.output("/usr/bin/systemd-run", &args.iter().map(String::as_str).collect::<Vec<_>>())
        .context("Service worker submission was not confirmed. Recheck progress before retrying. The request was not replayed")?;
    ensure!(result.status.success(), "Cannot start the independent service worker: {}. Check the host user manager, systemd-run and matching control helper, then recheck progress before retrying", result.stderr.trim());
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if read()?.is_some_and(|record| record.id == id) {
            return Ok(id);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("The service worker has not reported progress. Check the host user journal for {UNIT} and recheck progress before retrying. No action was replayed")
}

fn launch_arguments(
    context: &InstallationContext,
    request: ServiceActionRequest,
    helper: &Path,
    id: &str,
) -> Result<Vec<String>> {
    ensure!(valid_id(id), "Invalid service operation identifier");
    ensure!(helper.is_absolute(), "Service helper path must be absolute");
    let mut args: Vec<String> = [
        "--user",
        "--quiet",
        "--collect",
        "--no-ask-password",
        "--unit=lianli-control-operation.service",
        "--description=Lian Li service operation",
        "--property=Type=exec",
        "--property=Restart=no",
        "--property=KillMode=mixed",
        "--property=SendSIGKILL=no",
        "--property=TimeoutStopSec=120s",
        "--property=StandardInput=null",
        "--property=StandardOutput=journal",
        "--property=StandardError=journal",
        "--",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    match context {
        InstallationContext::Native => {}
        InstallationContext::Distrobox { name } => {
            ensure!(
                !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
                "Invalid Distrobox name"
            );
            args.extend([
                "/usr/bin/env".into(),
                "--unset=INVOCATION_ID".into(),
                "/usr/bin/distrobox-enter".into(),
                "--name".into(),
                name.clone(),
                "--".into(),
            ]);
        }
        InstallationContext::UnsupportedContainer => {
            anyhow::bail!("Service workers require a native or supported Distrobox installation")
        }
    }
    let command_start = args.iter().position(|arg| arg == "--").unwrap() + 1;
    args.extend([
        helper
            .to_str()
            .context("Service helper path is not UTF-8")?
            .into(),
        "run-service-action".into(),
        "--operation-id".into(),
        id.into(),
        "--scope".into(),
        request.scope.argument().trim_start_matches('-').into(),
        "--action".into(),
        request.action.argument().into(),
    ]);
    // systemd expands dollars in ExecStart arguments even though no shell is involved.
    for argument in &mut args[command_start..] {
        *argument = argument.replace('$', "$$");
    }
    Ok(args)
}

pub fn run(id: &str, request: ServiceActionRequest) -> Result<String> {
    ensure!(valid_id(id), "Invalid service operation identifier");
    run_at(&runtime_path()?, id, request, |progress| {
        crate::service_operation::execute(InstallationContext::detect(), request, progress)
    })
}

fn run_at(
    path: &Path,
    id: &str,
    request: ServiceActionRequest,
    operation: impl FnOnce(&mut dyn FnMut(&str) -> Result<()>) -> Result<String>,
) -> Result<String> {
    let directory = Directory::open(path, true)?.context("Missing service progress directory")?;
    let lock = directory.lock(true)?;
    ensure!(
        try_lock(&lock, libc::LOCK_EX)?,
        "Another service worker owns progress"
    );
    let _lock = WorkerLock(lock);
    let mut record = JobStatus {
        version: 1,
        updated_at_ms: 0,
        id: id.into(),
        request,
        status: ServiceOperationStatus {
            active: true,
            success: None,
            message: "Checking service state…".into(),
        },
    };
    directory.write(&record)?;
    let result = operation(&mut |message| {
        record.status.message = bounded_message(message);
        directory.write(&record)
    });
    record.status.active = false;
    record.status.success = Some(result.is_ok());
    record.status.message = bounded_message(&match &result {
        Ok(message) => message.clone(),
        Err(error) => format!("{error:#}"),
    });
    directory.write(&record).context(
        "Service action finished but its result could not be saved. Recheck actual services",
    )?;
    result
}

fn bounded_message(message: &str) -> String {
    message.chars().take(2048).collect()
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::services::{ServiceAction, ServiceScope};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn request() -> ServiceActionRequest {
        ServiceActionRequest {
            scope: ServiceScope::User,
            action: ServiceAction::Restart,
        }
    }

    #[test]
    fn observers_recover_progress_and_completion_without_owning_the_worker() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("progress");
        assert!(read_at(&path).unwrap().is_none());
        let result = run_at(&path, ID, request(), |progress| {
            progress("Verifying the new daemon instance")?;
            let first_gui = read_at(&path)?.unwrap();
            assert!(first_gui.status.active);
            assert_eq!(
                first_gui.status.message,
                "Verifying the new daemon instance"
            );
            drop(first_gui);
            assert!(read_at(&path)?.unwrap().status.active);
            assert!(run_at(&path, ID, request(), |_| panic!("Concurrent operation ran")).is_err());
            Ok("New instance verified".into())
        })
        .unwrap();
        assert_eq!(result, "New instance verified");
        let completed = read_at(&path).unwrap().unwrap();
        assert!(!completed.status.active);
        assert_eq!(completed.status.success, Some(true));
        assert_eq!(completed.status.message, result);
        assert_eq!(
            fs::metadata(path.join("status.json")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&path).unwrap().count(), 2);
    }

    #[test]
    fn failed_operation_retains_bounded_actionable_result() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("progress");
        assert!(run_at(&path, ID, request(), |_| anyhow::bail!(
            "Destination unavailable: {}",
            "x".repeat(32_000)
        ))
        .is_err());
        let record = read_at(&path).unwrap().unwrap();
        assert_eq!(record.status.success, Some(false));
        assert!(record
            .status
            .message
            .starts_with("Destination unavailable:"));
        assert!(fs::metadata(path.join("status.json")).unwrap().len() < MAX_BYTES);
    }

    #[test]
    fn abrupt_worker_exit_does_not_leave_permanent_busy_or_replay_an_action() {
        if let Some(path) = std::env::var_os("LIANLI_JOB_CRASH_FIXTURE") {
            let _ = run_at(Path::new(&path), ID, request(), |progress| {
                progress("Request sent. Verifying completion")?;
                std::process::exit(73);
            });
            panic!("The crash fixture did not execute");
        }
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("progress");
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "operation_job::tests::abrupt_worker_exit_does_not_leave_permanent_busy_or_replay_an_action"])
            .env("LIANLI_JOB_CRASH_FIXTURE", &path);
        let output = crate::command::run(command, Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(73));
        let raw = fs::read(path.join("status.json")).unwrap();
        let record = read_at(&path).unwrap().unwrap();
        assert!(!record.status.active);
        assert_eq!(record.status.success, Some(false));
        assert!(record.status.message.contains("interrupted"));
        assert!(record
            .status
            .message
            .contains("Recheck services before retrying"));
        assert_eq!(fs::read(path.join("status.json")).unwrap(), raw);
    }

    #[test]
    fn progress_rejects_symlinks_shared_directories_and_unbounded_input() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("progress");
        let directory = Directory::open(&path, true).unwrap().unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, "untouched").unwrap();
        symlink(&outside, path.join("worker.lock")).unwrap();
        assert!(run_at(&path, ID, request(), |_| panic!("Unsafe operation ran")).is_err());
        fs::remove_file(path.join("worker.lock")).unwrap();
        symlink(&outside, path.join("status.json")).unwrap();
        assert!(read_at(&path).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
        fs::remove_file(path.join("status.json")).unwrap();
        fs::write(path.join("status.json"), vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        fs::set_permissions(path.join("status.json"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(directory.read().is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(read_at(&path).is_err());
        let alias = temporary.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(Directory::open(&alias, false).is_err());
    }

    #[test]
    fn launch_keeps_arguments_literal_and_uses_a_detached_collectable_service() {
        let helper = Path::new("/a folder/$literal %value/lianli-control");
        let native = launch_arguments(&InstallationContext::Native, request(), helper, ID).unwrap();
        assert!(native.iter().any(|arg| arg == "--collect"));
        assert!(native.iter().any(|arg| arg == "--property=Restart=no"));
        let command = native.iter().position(|arg| arg == "--").unwrap() + 1;
        assert!(!native[..command]
            .iter()
            .any(|arg| ["--scope", "--pipe", "--pty", "--wait"].contains(&arg.as_str())));
        assert_eq!(
            &native[command..],
            [
                "/a folder/$$literal %value/lianli-control",
                "run-service-action",
                "--operation-id",
                ID,
                "--scope",
                "user",
                "--action",
                "restart"
            ]
        );
        let context = InstallationContext::Distrobox {
            name: "box with $literal".into(),
        };
        let container = launch_arguments(&context, request(), helper, ID).unwrap();
        assert_eq!(
            &container[command..command + 6],
            [
                "/usr/bin/env",
                "--unset=INVOCATION_ID",
                "/usr/bin/distrobox-enter",
                "--name",
                "box with $$literal",
                "--"
            ]
        );
        assert_eq!(
            container[command + 6],
            "/a folder/$$literal %value/lianli-control"
        );
        let system = launch_arguments(
            &context,
            ServiceActionRequest {
                scope: ServiceScope::System,
                ..request()
            },
            helper,
            ID,
        )
        .unwrap();
        assert!(system.windows(2).any(|pair| pair == ["--scope", "system"]));
        assert!(launch_arguments(
            &InstallationContext::UnsupportedContainer,
            request(),
            helper,
            ID
        )
        .is_err());
        assert!(launch_arguments(
            &InstallationContext::Native,
            request(),
            helper,
            "../invalid"
        )
        .is_err());
    }
}
