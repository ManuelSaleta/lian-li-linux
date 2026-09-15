use super::{find_owned_monitor, Control};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) struct Journal {
    file: File,
    path: PathBuf,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Session {
    uid: u32,
    pid: i32,
    device: u64,
    inode: u64,
}

impl From<&Control> for Session {
    fn from(control: &Control) -> Self {
        Self {
            uid: control.uid,
            pid: control.pid,
            device: control.device,
            inode: control.inode,
        }
    }
}

impl Journal {
    pub(super) fn create(control: &Control, name: &str) -> Result<Self> {
        let directory = directory(control)?;
        let _directory_lock = directory_lock(&directory)?;
        ensure!(
            fs::read_dir(&directory)?.take(65).count() < 64,
            "Too many Hyprland ownership records. Recover abandoned outputs first."
        );
        let path = directory.join(format!("{name}.staging"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        ensure!(
            lock(&file)?,
            "New Hyprland ownership record is already locked"
        );
        let mut journal = Self { file, path };
        journal
            .file
            .write_all(&serde_json::to_vec(&Session::from(control))?)?;
        journal.file.sync_all()?;
        journal.rename_extension("pending")?;
        Ok(journal)
    }

    pub(super) fn confirm(&mut self, id: u64) -> Result<()> {
        self.rename_extension(&format!("id{id}"))
    }

    fn rename_extension(&mut self, extension: &str) -> Result<()> {
        let destination = self.path.with_extension(extension);
        ensure!(
            !destination.try_exists()?,
            "Hyprland ownership record already exists"
        );
        fs::rename(&self.path, &destination)?;
        self.path = destination;
        Ok(())
    }

    pub(super) fn remove(&mut self) -> Result<()> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let owned = self.file.metadata()?;
        ensure!(
            metadata.dev() == owned.dev() && metadata.ino() == owned.ino(),
            "Hyprland ownership record was replaced"
        );
        fs::remove_file(&self.path)?;
        Ok(())
    }
}

impl Control {
    pub(super) fn recover_outputs(&self) -> Result<()> {
        let directory = directory(self)?;
        let _directory_lock = directory_lock(&directory)?;
        let deadline = Instant::now() + Duration::from_secs(4);
        for (index, entry) in fs::read_dir(directory)?.enumerate() {
            ensure!(
                index < 64,
                "Hyprland ownership directory exceeds 64 entries"
            );
            ensure!(
                Instant::now() < deadline,
                "Hyprland output recovery is incomplete. Retry startup."
            );
            let entry = entry?;
            let (name, id) = parse_name(&entry.file_name().to_string_lossy())?;
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(entry.path())
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file()
                    && metadata.uid() == self.uid
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o077 == 0
                    && metadata.len() <= 1024,
                "Invalid Hyprland ownership record"
            );
            if !lock(&file)? {
                continue;
            }
            let mut journal = Journal {
                file,
                path: entry.path(),
            };
            // Output creation cannot begin until the complete record has been published.
            if journal
                .path
                .extension()
                .is_some_and(|extension| extension == "staging")
            {
                journal.remove()?;
                continue;
            }
            let mut bytes = Vec::new();
            (&journal.file).take(1025).read_to_end(&mut bytes)?;
            let session: Session =
                serde_json::from_slice(&bytes).context("Invalid Hyprland ownership identity")?;
            ensure!(
                session == Session::from(self),
                "Hyprland ownership belongs to another compositor instance"
            );
            if find_owned_monitor(&self.monitors()?, &name, id)?.is_some() {
                ensure!(
                    self.request(&format!("/output remove {name}"))? == b"ok",
                    "Hyprland refused abandoned-output removal"
                );
                tracing::info!("Removed abandoned owned Hyprland output {name}");
            }
            journal.remove()?;
        }
        Ok(())
    }
}

fn directory(control: &Control) -> Result<PathBuf> {
    let parent = control
        .path
        .parent()
        .context("Hyprland socket has no directory")?;
    validate_directory(parent, control.uid, false)?;
    let path = parent.join("lianli-owned-outputs");
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    validate_directory(&path, control.uid, true)?;
    Ok(path)
}

fn validate_directory(path: &Path, uid: u32, private: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir() && metadata.uid() == uid && metadata.mode() & if private { 0o077 } else { 0o022 } == 0,
        "Hyprland ownership directory must be owned by the session user and protected from other users");
    Ok(())
}

fn directory_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(
        lock(&file)?,
        "Another display worker is updating Hyprland ownership. Retry startup."
    );
    Ok(file)
}

fn lock(file: &File) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error.into())
}

fn parse_name(filename: &str) -> Result<(String, Option<u64>)> {
    let (name, suffix) = filename
        .rsplit_once('.')
        .context("Invalid Hyprland ownership filename")?;
    ensure!(
        name.starts_with("LianLi-")
            && name.len() == 31
            && name[7..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid owned output name"
    );
    let id = if matches!(suffix, "pending" | "staging") {
        None
    } else {
        Some(
            suffix
                .strip_prefix("id")
                .context("Invalid Hyprland output ID")?
                .parse()?,
        )
    };
    Ok((name.to_owned(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn recovery_removes_only_abandoned_outputs_and_refuses_changed_identity() {
        for changed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(".socket.sock");
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let control = Control {
                path,
                uid: unsafe { libc::geteuid() },
                pid: unsafe { libc::getpid() },
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            let name = "LianLi-0123456789abcdef01234567";
            let mut abandoned = Journal::create(&control, name).unwrap();
            abandoned.confirm(9).unwrap();
            let record = abandoned.path.clone();
            drop(abandoned);
            let live = Journal::create(&control, "LianLi-aaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
            let monitors = serde_json::json!([
                { "id": if changed { 10 } else { 9 }, "name": name, "width": 480, "height": 480, "refreshRate": 30.0 },
                { "id": 20, "name": "HEADLESS-1", "width": 1920, "height": 1080, "refreshRate": 60.0 }
            ]);
            let mut replies = vec![(
                "j/monitors all".to_owned(),
                serde_json::to_vec(&monitors).unwrap(),
            )];
            if !changed {
                replies.push((format!("/output remove {name}"), b"ok".to_vec()));
            }
            let server = std::thread::spawn(move || {
                for (expected, reply) in replies {
                    crate::socket::wait(
                        listener.as_raw_fd(),
                        libc::POLLIN,
                        Instant::now() + Duration::from_secs(3),
                    )
                    .unwrap();
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = vec![0; expected.len()];
                    stream.read_exact(&mut request).unwrap();
                    assert_eq!(request, expected.as_bytes());
                    stream.write_all(&reply).unwrap();
                }
            });
            let result = control.recover_outputs();
            server.join().unwrap();
            assert_eq!(result.is_err(), changed);
            assert_eq!(record.exists(), changed);
            assert!(live.path.exists());
        }
    }

    #[test]
    fn ownership_survives_confirmation_and_cannot_be_taken_from_a_live_worker() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control {
            path: dir.path().join(".socket.sock"),
            uid: unsafe { libc::geteuid() },
            pid: 2,
            device: 3,
            inode: 4,
        };
        let name = "LianLi-0123456789abcdef01234567";
        let mut owner = Journal::create(&control, name).unwrap();
        let stale_path = owner.path.clone();
        owner.confirm(9).unwrap();
        assert!(!stale_path.exists());
        assert_eq!(
            parse_name(owner.path.file_name().unwrap().to_str().unwrap()).unwrap(),
            (name.into(), Some(9))
        );
        let competitor = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&owner.path)
            .unwrap();
        assert!(!lock(&competitor).unwrap());
        let path = owner.path.clone();
        drop(owner);
        assert!(lock(&competitor).unwrap());
        let mut recovered = Journal {
            file: competitor,
            path,
        };
        recovered.remove().unwrap();
        assert!(!recovered.path.exists());
        let staged = recovered.path.with_extension("staging");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)
            .unwrap();
        control.recover_outputs().unwrap();
        assert!(!staged.exists());
        for name in [
            "HEADLESS-1.id1",
            "LianLi-../anything.pending",
            "LianLi-0123456789abcdef01234567.id-1",
        ] {
            assert!(parse_name(name).is_err());
        }
    }
}
