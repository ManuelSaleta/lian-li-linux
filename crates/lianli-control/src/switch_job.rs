use crate::account::Account;
use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::{ServiceChangeRequest, ServiceOperationStatus};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DIRECTORY: &str = "/run/lianli-switch";
const HELPER: &str = "/usr/bin/lianli-control";
const LIMIT: u64 = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStatus {
    version: u32,
    pub id: String,
    pub caller_uid: u32,
    pub request: ServiceChangeRequest,
    pub updated_at_ms: u64,
    pub status: ServiceOperationStatus,
}

struct Store {
    directory: File,
    uid: u32,
}

struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Store {
    fn open(path: &Path, uid: u32, create: bool) -> Result<Option<Self>> {
        if create {
            ensure!(
                uid == unsafe { libc::geteuid() },
                "Wrong progress writer account"
            );
            match fs::DirBuilder::new().mode(0o755).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let directory = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = directory.metadata()?;
        ensure!(
            metadata.uid() == uid && metadata.mode() & 0o022 == 0,
            "Switch progress directory has unsafe ownership or permissions"
        );
        if create {
            directory.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        Ok(Some(Self { directory, uid }))
    }

    fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(name)
    }

    fn file(&self, name: &str, create: bool) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(create)
            .create(create)
            .truncate(false)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path(name))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == self.uid
                && metadata.mode() & 0o022 == 0
                && metadata.nlink() == 1
                && metadata.len() <= LIMIT,
            "Invalid switch progress file"
        );
        if create {
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        Ok(file)
    }

    fn read(&self) -> Result<Option<JobStatus>> {
        let file = match self.file("status.json", false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= LIMIT,
            "Switch progress exceeds its size limit"
        );
        let record: JobStatus = serde_json::from_slice(&bytes)?;
        validate_id(&record.id)?;
        ensure!(
            record.version == 1
                && record.caller_uid != 0
                && record.status.active == record.status.success.is_none(),
            "Invalid switch progress record"
        );
        Ok(Some(record))
    }

    fn write(&self, record: &JobStatus) -> Result<()> {
        let mut record = record.clone();
        record.updated_at_ms = timestamp();
        let bytes = serde_json::to_vec(&record)?;
        ensure!(
            bytes.len() as u64 <= LIMIT,
            "Switch progress exceeds its size limit"
        );
        let mut file = tempfile::NamedTempFile::new_in(self.path(""))?;
        file.write_all(&bytes)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        file.as_file().sync_all()?;
        file.persist(self.path("status.json"))?;
        self.directory.sync_all()?;
        Ok(())
    }
}

pub(crate) fn timestamp() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(time.tv_nsec as u64 / 1_000_000)
}

fn validate_id(id: &str) -> Result<()> {
    lianli_shared::daemon::parse_service_invocation(id)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn try_lock(file: &File, mode: i32) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error.into())
}

pub fn read() -> Result<Option<JobStatus>> {
    read_at(Path::new(DIRECTORY), 0)
}

fn read_at(path: &Path, uid: u32) -> Result<Option<JobStatus>> {
    let Some(store) = Store::open(path, uid, false)? else {
        return Ok(None);
    };
    let Some(mut record) = store.read()? else {
        return Ok(None);
    };
    if record.status.active {
        let file = store.file("worker.lock", false)?;
        if try_lock(&file, libc::LOCK_SH)? {
            let _lock = Lock(file);
            record = store.read()?.context("Switch progress disappeared")?;
            if record.status.active {
                record.status = ServiceOperationStatus {
                    active: false, success: Some(false),
                    message: "Service switching was interrupted. Use Recover switch before trying another mode.".into(),
                };
            }
        }
    }
    Ok(Some(record))
}

fn request_arguments(request: ServiceChangeRequest) -> Vec<String> {
    match request {
        ServiceChangeRequest::Recover {} => vec!["--recover".into()],
        ServiceChangeRequest::Switch {
            scope,
            carry_settings,
        } => {
            let mut args = vec![
                "--scope".into(),
                scope.argument().trim_start_matches('-').into(),
            ];
            if carry_settings {
                args.push("--carry-settings".into());
            }
            args
        }
    }
}

fn installed_helper() -> Result<()> {
    for path in ["/usr", "/usr/bin", HELPER] {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("Native service helper path is unavailable: {path}"))?;
        ensure!(
            metadata.uid() == 0
                && metadata.mode() & 0o022 == 0
                && if path == HELPER {
                    metadata.is_file() && metadata.mode() & 0o111 != 0
                } else {
                    metadata.is_dir()
                },
            "Install the root-owned native control helper before switching services"
        );
    }
    Ok(())
}

pub fn prerequisites() -> Result<()> {
    installed_helper()?;
    for path in ["/usr/bin/pkexec", "/usr/bin/systemd-run"] {
        let metadata = fs::metadata(path)
            .with_context(|| format!("Required service-switch tool is missing: {path}"))?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.mode() & 0o111 != 0
                && metadata.mode() & 0o022 == 0,
            "Service-switch tool must be executable and root-owned: {path}"
        );
    }
    Ok(())
}

pub fn start(request: ServiceChangeRequest) -> Result<String> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native
            && unsafe { libc::geteuid() } != 0,
        "Switch service modes from the native desktop application"
    );
    prerequisites()?;
    ensure!(
        !read()?.is_some_and(|record| record.status.active),
        "Another service switch is running"
    );
    ensure!(
        !crate::operation_job::read()?.is_some_and(|record| record.status.active),
        "Another service action is running"
    );
    let mut random = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut command = Command::new("/usr/bin/pkexec");
    command.args([
        "--disable-internal-agent",
        HELPER,
        "submit-switch",
        "--operation-id",
        &id,
    ]);
    command.args(request_arguments(request));
    let output = crate::command::run(command, Duration::from_secs(180))
        .context("Switch submission was not confirmed. Recheck progress before retrying. No request was replayed")?;
    ensure!(output.status.success(), "Cannot submit service switch: {}. Check administrator authorization and the installed native helper", output.stderr.trim());
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if read()?.is_some_and(|record| {
            record.id == id
                && record.caller_uid == unsafe { libc::geteuid() }
                && record.request == request
        }) {
            return Ok(id);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!(
        "Switch worker has not reported progress. Recheck before retrying. No request was replayed"
    )
}

fn launch_arguments(uid: u32, id: &str, request: ServiceChangeRequest) -> Result<Vec<String>> {
    validate_id(id)?;
    ensure!(uid != 0, "A desktop caller account is required");
    let mut args: Vec<String> = [
        "--system",
        "--quiet",
        "--collect",
        "--no-ask-password",
        "--unit=lianli-control-switch.service",
        "--description=Lian Li service mode switch",
        "--property=Type=exec",
        "--property=Restart=no",
        "--property=KillMode=mixed",
        "--property=SendSIGKILL=no",
        "--property=TimeoutStopSec=120s",
        "--property=StandardInput=null",
        "--property=StandardOutput=journal",
        "--property=StandardError=journal",
        "--",
        HELPER,
        "run-switch",
        "--operation-id",
        id,
        "--caller-uid",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(uid.to_string());
    args.extend(request_arguments(request));
    Ok(args)
}

pub fn submit(id: &str, request: ServiceChangeRequest) -> Result<()> {
    let caller = Account::authorized_caller()?;
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Switch workers require a native installation"
    );
    installed_helper()?;
    let mut command = Command::new("/usr/bin/systemd-run");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C");
    command.args(launch_arguments(caller.uid, id, request)?);
    let output = crate::command::run(command, Duration::from_secs(30))?;
    ensure!(
        output.status.success(),
        "Cannot submit the system switch worker: {}",
        output.stderr.trim()
    );
    Ok(())
}

pub fn run(uid: u32, id: &str, request: ServiceChangeRequest) -> Result<String> {
    ensure!(
        unsafe { libc::geteuid() } == 0
            && InstallationContext::detect() == InstallationContext::Native,
        "The native system manager must launch the switch worker"
    );
    let caller = Account::user(uid)?;
    run_at(
        Path::new(DIRECTORY),
        0,
        uid,
        id,
        request,
        |progress| match request {
            ServiceChangeRequest::Switch {
                scope,
                carry_settings,
            } => crate::native_switch::execute_for(caller, scope, carry_settings, progress),
            ServiceChangeRequest::Recover {} => crate::native_switch::recover_for(caller, progress),
        },
    )
}

fn run_at(
    path: &Path,
    owner: u32,
    caller: u32,
    id: &str,
    request: ServiceChangeRequest,
    operation: impl FnOnce(&mut dyn FnMut(&str) -> Result<()>) -> Result<String>,
) -> Result<String> {
    validate_id(id)?;
    let store = Store::open(path, owner, true)?.context("Missing switch progress directory")?;
    let lock = store.file("worker.lock", true)?;
    ensure!(
        try_lock(&lock, libc::LOCK_EX)?,
        "Another switch worker owns progress"
    );
    let _lock = Lock(lock);
    let mut record = JobStatus {
        version: 1,
        id: id.into(),
        caller_uid: caller,
        request,
        updated_at_ms: 0,
        status: ServiceOperationStatus {
            active: true,
            success: None,
            message: "Checking service switch prerequisites…".into(),
        },
    };
    store.write(&record)?;
    let result = operation(&mut |message| {
        record.status.message = message.chars().take(2048).collect();
        store.write(&record)
    });
    record.status.active = false;
    record.status.success = Some(result.is_ok());
    // Public progress contains no paths or private configuration details from failures.
    record.status.message = match &result {
        Ok(message) => message.chars().take(2048).collect(),
        Err(_) => "Service switch failed. Recheck Installation Health. If recovery is pending, choose Recover switch. See the journal for lianli-control-switch.service or lianli-control-recovery.service.".into(),
    };
    store
        .write(&record)
        .context("Switch finished but progress could not be saved. Recheck actual services")?;
    result
}

pub(crate) fn recover_automatically(caller: Account, id: &str) -> Result<String> {
    ensure!(
        unsafe { libc::geteuid() } == 0
            && InstallationContext::detect() == InstallationContext::Native,
        "Automatic recovery requires the native system service"
    );
    run_at(
        Path::new(DIRECTORY),
        0,
        caller.uid,
        id,
        ServiceChangeRequest::Recover {},
        |progress| crate::native_switch::recover_matching(caller, Some(id), progress),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::services::ServiceScope;
    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn submission_uses_only_fixed_system_service_and_typed_request() {
        let request = ServiceChangeRequest::Switch {
            scope: ServiceScope::System,
            carry_settings: true,
        };
        let args = launch_arguments(1000, ID, request).unwrap();
        let command = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            &args[command + 1..],
            &[
                HELPER,
                "run-switch",
                "--operation-id",
                ID,
                "--caller-uid",
                "1000",
                "--scope",
                "system",
                "--carry-settings"
            ]
        );
        assert!(args.contains(&"--property=Type=exec".into()));
        assert!(args.contains(&"--property=SendSIGKILL=no".into()));
        assert_eq!(
            request_arguments(ServiceChangeRequest::Recover {}),
            ["--recover"]
        );
        assert!(launch_arguments(0, ID, request).is_err());
        assert!(launch_arguments(1000, "../../other", request).is_err());
    }

    #[test]
    fn independent_observers_see_progress_completion_and_redacted_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("progress");
        let uid = unsafe { libc::geteuid() };
        let request = ServiceChangeRequest::Recover {};
        run_at(&path, uid, 1000, ID, request, |progress| {
            progress("Restoring the previous service mode…")?;
            let observed = read_at(&path, uid)?.unwrap();
            assert!(observed.status.active);
            assert!(observed.status.message.starts_with("Restoring"));
            assert!(run_at(&path, uid, 1000, ID, request, |_| panic!(
                "Concurrent operation ran"
            ))
            .is_err());
            Ok("Previous service mode restored and verified.".into())
        })
        .unwrap();
        assert_eq!(
            read_at(&path, uid).unwrap().unwrap().status.success,
            Some(true)
        );
        assert!(run_at(&path, uid, 1000, ID, request, |_| anyhow::bail!(
            "/home/private/secret.png"
        ))
        .is_err());
        let failed = read_at(&path, uid).unwrap().unwrap();
        assert_eq!(failed.status.success, Some(false));
        assert!(!failed.status.message.contains("secret"));
        assert!(failed.updated_at_ms > 0);
    }

    #[test]
    fn interrupted_progress_and_unsafe_storage_are_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("progress");
        let uid = unsafe { libc::geteuid() };
        let store = Store::open(&path, uid, true).unwrap().unwrap();
        let _file = store.file("worker.lock", true).unwrap();
        let record = JobStatus {
            version: 1,
            id: ID.into(),
            caller_uid: 1000,
            request: ServiceChangeRequest::Recover {},
            updated_at_ms: 0,
            status: ServiceOperationStatus {
                active: true,
                success: None,
                message: "Working".into(),
            },
        };
        store.write(&record).unwrap();
        let observed = read_at(&path, uid).unwrap().unwrap();
        assert_eq!(observed.status.success, Some(false));
        assert!(observed.status.message.contains("interrupted"));
        fs::set_permissions(path.join("status.json"), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_at(&path, uid).is_err());
        fs::remove_file(path.join("status.json")).unwrap();
        std::os::unix::fs::symlink(temporary.path().join("outside"), path.join("status.json"))
            .unwrap();
        assert!(read_at(&path, uid).is_err());
    }
}
