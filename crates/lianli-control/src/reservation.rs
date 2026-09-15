use crate::ownership::{file_identity, host_identity_at};
use crate::services::Route;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::FileIdentity;
use lianli_shared::installation::{
    InstallationContext, DAEMON_LOCK_PATH, SERVICE_OPERATION_LOCK_PATH,
};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Keeps every cooperating daemon stopped while destination state is published.
/// Drop the reservation before starting a daemon, then verify that daemon's identity.
pub struct HardwareReservation {
    file: File,
    path: PathBuf,
    identity: FileIdentity,
    route: Route,
    require_root: bool,
    host_path: &'static str,
}

impl HardwareReservation {
    /// The expected identity must come from the operation's fresh host ownership check.
    /// This never stops a running owner, creates a lock file or changes its PID text.
    pub fn acquire(context: &InstallationContext, expected: &FileIdentity) -> Result<Self> {
        let route = Route::detect(context)?;
        let path = context
            .daemon_lock_path()
            .context("Shared host lock is unavailable")?;
        Self::open(
            path,
            expected,
            route,
            matches!(context, InstallationContext::Native),
        )
    }

    fn open(
        path: PathBuf,
        expected: &FileIdentity,
        route: Route,
        require_root: bool,
    ) -> Result<Self> {
        Self::open_with_host_path(path, expected, route, require_root, DAEMON_LOCK_PATH)
    }

    fn open_with_host_path(
        path: PathBuf,
        expected: &FileIdentity,
        route: Route,
        require_root: bool,
        host_path: &'static str,
    ) -> Result<Self> {
        Self::open_locked(
            path,
            expected,
            route,
            require_root,
            host_path,
            libc::LOCK_EX,
            Duration::ZERO,
        )
    }

    pub(crate) fn shared_control(
        path: PathBuf,
        expected: &FileIdentity,
        require_root: bool,
    ) -> Result<Self> {
        // The operation checks the daemon's reported identity; IPC writes must not spawn host commands.
        Self::open_locked(
            path,
            expected,
            Route::Native,
            require_root,
            SERVICE_OPERATION_LOCK_PATH,
            libc::LOCK_SH,
            Duration::ZERO,
        )
    }

    fn open_locked(
        path: PathBuf,
        expected: &FileIdentity,
        route: Route,
        require_root: bool,
        host_path: &'static str,
        lock_mode: libc::c_int,
        wait: Duration,
    ) -> Result<Self> {
        ensure!(
            file_identity(&path, require_root)? == *expected,
            "Hardware ownership lock changed before reservation"
        );
        if matches!(route, Route::Host { .. }) {
            ensure!(
                host_identity_at(&route, host_path)? == *expected,
                "Host and container ownership locks differ"
            );
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && (!require_root || metadata.uid() == 0),
            "Invalid hardware ownership lock"
        );
        let identity = FileIdentity {
            device: metadata.dev().to_string(),
            inode: metadata.ino().to_string(),
        };
        ensure!(
            identity == *expected,
            "Hardware ownership lock was replaced while opening"
        );
        // The owned descriptor retains the flock until the reservation is dropped.
        let deadline = Instant::now() + wait;
        while unsafe { libc::flock(file.as_raw_fd(), lock_mode | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock || Instant::now() >= deadline {
                return Err(error).context(
                    "The shared lock is busy or unavailable. Wait for its current owner to finish",
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let reservation = Self {
            file,
            path,
            identity,
            route,
            require_root,
            host_path,
        };
        reservation.verify()?;
        Ok(reservation)
    }

    pub fn verify(&self) -> Result<()> {
        let held = self.file.metadata()?;
        ensure!(
            held.dev().to_string() == self.identity.device
                && held.ino().to_string() == self.identity.inode,
            "Reserved hardware lock identity changed"
        );
        ensure!(
            file_identity(&self.path, self.require_root)? == self.identity,
            "Reserved hardware lock path was replaced. Refusing state publication"
        );
        if matches!(self.route, Route::Host { .. }) {
            ensure!(
                host_identity_at(&self.route, self.host_path)? == self.identity,
                "Reserved lock no longer matches the host lock"
            );
        }
        Ok(())
    }

    pub(crate) fn publication_descriptor(&self) -> Result<BorrowedFd<'_>> {
        ensure!(
            self.host_path == DAEMON_LOCK_PATH && matches!(self.route, Route::Native),
            "Publication requires the native hardware reservation"
        );
        self.verify()?;
        Ok(self.file.as_fd())
    }
}

pub struct ServiceOperationLock(HardwareReservation);

impl Drop for HardwareReservation {
    fn drop(&mut self) {
        // A fork-to-exec child may still hold a copy; closing only this descriptor delays release.
        // Closing the file remains the fallback if explicit unlocking fails.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) fn operation_identity(
    context: &InstallationContext,
    route: &Route,
) -> Result<FileIdentity> {
    let path = context
        .service_operation_lock_path()
        .context("Host service operation lock is unavailable")?;
    let require_root = matches!(context, InstallationContext::Native);
    let identity = file_identity(&path, require_root)?;
    if matches!(route, Route::Host { .. }) {
        ensure!(
            host_identity_at(route, SERVICE_OPERATION_LOCK_PATH)? == identity,
            "Host and container service operation locks differ"
        );
    }
    ensure!(
        file_identity(&path, require_root)? == identity,
        "Service operation lock changed during inspection"
    );
    Ok(identity)
}

impl ServiceOperationLock {
    pub(crate) fn identity(&self) -> &FileIdentity {
        &self.0.identity
    }
    pub fn acquire(context: &InstallationContext) -> Result<Self> {
        let path = context
            .service_operation_lock_path()
            .context("Host service operation lock is unavailable")?;
        let require_root = matches!(context, InstallationContext::Native);
        let identity = file_identity(&path, require_root).context(
            "Install the current tmpfiles rule and run sudo systemd-tmpfiles --create lianli.conf on the host before using service controls")?;
        HardwareReservation::open_locked(
            path,
            &identity,
            Route::detect(context)?,
            require_root,
            SERVICE_OPERATION_LOCK_PATH,
            libc::LOCK_EX,
            Duration::from_secs(5),
        )
        .map(Self)
        .context(
            "Cannot serialize service actions. Another operation or settings write may be running",
        )
    }

    pub fn verify(&self) -> Result<()> {
        self.0.verify()
    }

    pub(crate) fn publication_descriptor(&self) -> Result<BorrowedFd<'_>> {
        ensure!(
            matches!(self.0.route, Route::Native),
            "Publication requires the native operation reservation"
        );
        self.verify()?;
        Ok(self.0.file.as_fd())
    }
}

// Close inherited descriptors without LOCK_UN, which would release the coordinator's lock too.
pub(crate) struct InheritedReservations {
    operation: File,
    hardware: File,
    paths: [PathBuf; 2],
    require_root: bool,
    route: Route,
}

impl InheritedReservations {
    pub fn new(operation: File, hardware: File) -> Result<Self> {
        let context = InstallationContext::detect();
        Self::at_with_route(
            operation,
            hardware,
            [
                context
                    .service_operation_lock_path()
                    .context("Host service operation lock is unavailable")?,
                context
                    .daemon_lock_path()
                    .context("Shared host lock is unavailable")?,
            ],
            matches!(context, InstallationContext::Native),
            Route::detect(&context)?,
        )
    }

    #[cfg(test)]
    pub(crate) fn at(
        operation: File,
        hardware: File,
        paths: [PathBuf; 2],
        require_root: bool,
    ) -> Result<Self> {
        Self::at_with_route(operation, hardware, paths, require_root, Route::Native)
    }

    fn at_with_route(
        operation: File,
        hardware: File,
        paths: [PathBuf; 2],
        require_root: bool,
        route: Route,
    ) -> Result<Self> {
        let locks = Self {
            operation,
            hardware,
            paths,
            require_root,
            route,
        };
        locks.verify()?;
        Ok(locks)
    }

    pub fn verify(&self) -> Result<()> {
        for ((file, path), host_path) in [&self.operation, &self.hardware]
            .into_iter()
            .zip(&self.paths)
            .zip([SERVICE_OPERATION_LOCK_PATH, DAEMON_LOCK_PATH])
        {
            let metadata = file.metadata()?;
            let expected = file_identity(path, self.require_root)?;
            if matches!(self.route, Route::Host { .. }) {
                ensure!(
                    host_identity_at(&self.route, host_path)? == expected,
                    "Inherited publication reservation no longer matches the host namespace"
                );
            }
            ensure!(
                metadata.is_file()
                    && (!self.require_root || metadata.uid() == 0)
                    && metadata.dev().to_string() == expected.device
                    && metadata.ino().to_string() == expected.inode,
                "Inherited publication reservation no longer matches its host lock"
            );
            ensure!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                "Publication cannot retain exclusive ownership: {}",
                std::io::Error::last_os_error()
            );
        }
        ensure!(
            self.operation.metadata()?.ino() != self.hardware.metadata()?.ino()
                || self.operation.metadata()?.dev() != self.hardware.metadata()?.dev(),
            "Publication requires two distinct reservations"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_reservations_hold_exclusion_without_unlocking_the_coordinator() {
        let root = tempfile::tempdir().unwrap();
        let paths = [root.path().join("operation"), root.path().join("hardware")];
        for path in &paths {
            std::fs::write(path, "pid text").unwrap();
        }
        let reserve = |path: &std::path::Path| {
            HardwareReservation::open(
                path.into(),
                &file_identity(path, false).unwrap(),
                Route::Native,
                false,
            )
        };
        let operation = reserve(&paths[0]).unwrap();
        let hardware = reserve(&paths[1]).unwrap();
        let inherited = InheritedReservations::at(
            operation.file.try_clone().unwrap(),
            hardware.file.try_clone().unwrap(),
            paths.clone(),
            false,
        )
        .unwrap();
        let busy = |path: &std::path::Path| reserve(path).is_err();
        assert!(paths.iter().all(|path| busy(path)));
        drop(inherited);
        assert!(paths.iter().all(|path| busy(path)));
        drop(operation);
        assert!(!busy(&paths[0]));
        assert!(busy(&paths[1]));
        drop(hardware);
        assert!(!busy(&paths[1]));
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), "pid text");
    }

    #[test]
    fn inherited_reservations_reject_other_owners_and_replaced_paths() {
        let root = tempfile::tempdir().unwrap();
        let paths = [root.path().join("operation"), root.path().join("hardware")];
        for path in &paths {
            std::fs::write(path, "").unwrap();
        }
        let operation = File::open(&paths[0]).unwrap();
        let hardware = File::open(&paths[1]).unwrap();
        let inherited = InheritedReservations::at(
            operation.try_clone().unwrap(),
            hardware.try_clone().unwrap(),
            paths.clone(),
            false,
        )
        .unwrap();
        assert!(InheritedReservations::at(
            File::open(&paths[0]).unwrap(),
            File::open(&paths[1]).unwrap(),
            paths.clone(),
            false
        )
        .is_err());
        std::fs::remove_file(&paths[1]).unwrap();
        std::fs::write(&paths[1], "replacement").unwrap();
        assert!(inherited.verify().is_err());
    }

    #[test]
    fn service_lock_waits_for_an_existing_writer_and_times_out_without_interrupting_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control");
        std::fs::write(&path, "").unwrap();
        let identity = file_identity(&path, false).unwrap();
        let writer = HardwareReservation::shared_control(path.clone(), &identity, false).unwrap();
        assert!(HardwareReservation::open_locked(
            path.clone(),
            &identity,
            Route::Native,
            false,
            SERVICE_OPERATION_LOCK_PATH,
            libc::LOCK_EX,
            Duration::from_millis(20)
        )
        .is_err());
        let (completed, completion) = std::sync::mpsc::channel();
        let (began, beginning) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            began.send(()).unwrap();
            let lock = HardwareReservation::open_locked(
                path,
                &identity,
                Route::Native,
                false,
                SERVICE_OPERATION_LOCK_PATH,
                libc::LOCK_EX,
                Duration::from_secs(2),
            );
            assert!(completed.send(lock).is_ok());
        });
        beginning.recv_timeout(Duration::from_secs(1)).unwrap();
        let premature = completion.recv_timeout(Duration::from_millis(30));
        drop(writer);
        worker.join().unwrap();
        assert!(matches!(
            premature,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(completion
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok());
    }

    #[test]
    fn service_actions_serialize_without_taking_the_running_daemons_lock() {
        let root = tempfile::tempdir().unwrap();
        let hardware = root.path().join("hardware");
        let operation = root.path().join("operation");
        std::fs::write(&hardware, "daemon PID").unwrap();
        std::fs::write(&operation, "").unwrap();
        let hardware_id = file_identity(&hardware, false).unwrap();
        let operation_id = file_identity(&operation, false).unwrap();
        let daemon =
            HardwareReservation::open(hardware, &hardware_id, Route::Native, false).unwrap();
        let action = ServiceOperationLock(
            HardwareReservation::open_with_host_path(
                operation.clone(),
                &operation_id,
                Route::Native,
                false,
                SERVICE_OPERATION_LOCK_PATH,
            )
            .unwrap(),
        );
        assert!(action.verify().is_ok());
        assert!(daemon.verify().is_ok());
        assert!(HardwareReservation::open_with_host_path(
            operation.clone(),
            &operation_id,
            Route::Native,
            false,
            SERVICE_OPERATION_LOCK_PATH
        )
        .is_err());
        drop(action);
        assert!(HardwareReservation::open_with_host_path(
            operation,
            &operation_id,
            Route::Native,
            false,
            SERVICE_OPERATION_LOCK_PATH
        )
        .is_ok());
    }

    #[test]
    fn reservation_excludes_daemon_opens_and_preserves_pid_text() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        std::fs::write(&path, "123\n").unwrap();
        let identity = file_identity(&path, false).unwrap();
        let reservation =
            HardwareReservation::open(path.clone(), &identity, Route::Native, false).unwrap();
        assert!(HardwareReservation::open(path.clone(), &identity, Route::Native, false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "123\n");
        drop(reservation);
        assert!(HardwareReservation::open(path, &identity, Route::Native, false).is_ok());
    }

    #[test]
    fn dropping_a_reservation_releases_the_lock_despite_an_inherited_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        std::fs::write(&path, "123\n").unwrap();
        let identity = file_identity(&path, false).unwrap();
        let reservation =
            HardwareReservation::open(path.clone(), &identity, Route::Native, false).unwrap();
        let inherited = reservation.file.try_clone().unwrap();
        drop(reservation);
        let next =
            HardwareReservation::open(path.clone(), &identity, Route::Native, false).unwrap();
        drop(inherited);
        assert!(HardwareReservation::open(path.clone(), &identity, Route::Native, false).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "123\n");
        drop(next);
    }

    #[test]
    fn missing_symlinked_and_replaced_lock_paths_refuse_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        std::fs::write(&path, "").unwrap();
        let identity = file_identity(&path, false).unwrap();
        let reservation =
            HardwareReservation::open(path.clone(), &identity, Route::Native, false).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(reservation.verify().is_err());
        assert!(HardwareReservation::open(path.clone(), &identity, Route::Native, false).is_err());
        assert!(!path.exists());
        std::fs::write(&path, "replacement").unwrap();
        assert!(reservation.verify().is_err());
        assert!(HardwareReservation::open(path.clone(), &identity, Route::Native, false).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(root.path().join("other"), &path).unwrap();
        assert!(reservation.verify().is_err());
        assert!(HardwareReservation::open(path, &identity, Route::Native, false).is_err());
    }
}
