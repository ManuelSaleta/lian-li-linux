use crate::socket;
mod journal;
use anyhow::{bail, ensure, Context, Result};
use journal::Journal;
use serde::Deserialize;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REPLY_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub id: u64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[serde(rename = "refreshRate")]
    pub refresh_hz: f64,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone)]
pub struct Control {
    path: PathBuf,
    uid: u32,
    pid: i32,
    device: u64,
    inode: u64,
}

impl Control {
    pub(crate) fn wayland_stream(&self) -> Result<UnixStream> {
        let runtime =
            std::env::var_os("XDG_RUNTIME_DIR").context("No graphical runtime directory")?;
        let display = std::env::var_os("WAYLAND_DISPLAY").context("No Wayland display")?;
        let path = Path::new(&runtime).join(display);
        let stream = socket::connect(&path, Instant::now() + CONTROL_TIMEOUT)?;
        let peer = socket::credentials(&stream)?;
        ensure!(
            peer.uid == self.uid && peer.pid == self.pid,
            "Wayland and Hyprland control belong to different sessions"
        );
        Ok(stream)
    }

    pub fn from_env() -> Result<Self> {
        let runtime =
            std::env::var_os("XDG_RUNTIME_DIR").context("No graphical runtime directory")?;
        let signature =
            std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("No Hyprland session")?;
        let path = control_path(Path::new(&runtime), &signature)?;
        let control = Self::connect(path)?;
        control.recover_outputs()?;
        Ok(control)
    }

    fn connect(path: PathBuf) -> Result<Self> {
        Self::connect_before(path, Instant::now() + CONTROL_TIMEOUT)
    }

    pub fn verify_session(
        runtime: &Path,
        signature: &str,
        display: &str,
        deadline: Instant,
    ) -> Result<()> {
        let control = Self::connect_before(control_path(runtime, signature)?, deadline)?;
        let wayland = socket::connect(&runtime.join(display), deadline)?;
        let peer = socket::credentials(&wayland)?;
        ensure!(
            peer.uid == control.uid && peer.pid == control.pid,
            "Wayland and Hyprland control belong to different sessions"
        );
        Ok(())
    }

    fn connect_before(path: PathBuf, deadline: Instant) -> Result<Self> {
        let metadata =
            fs::symlink_metadata(&path).context("Hyprland control socket is unavailable")?;
        let mut stream = socket::connect(&path, deadline)?;
        let peer = socket::credentials(&stream)?;
        ensure!(
            peer.uid == unsafe { libc::geteuid() },
            "Hyprland belongs to another user"
        );
        let control = Self {
            path,
            uid: peer.uid,
            pid: peer.pid,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        // Hyprland handles requests serially; an idle credential probe blocks later requests.
        let reply =
            exchange(&mut stream, b"j/version", deadline).context("Querying Hyprland version")?;
        let version: serde_json::Value =
            serde_json::from_slice(&reply).context("Invalid Hyprland version response")?;
        validate_version(&version)?;
        Ok(control)
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        serde_json::from_slice(&self.request("j/monitors all")?)
            .context("Invalid Hyprland monitor response")
    }

    pub fn create_output(&self) -> Result<OwnedOutput> {
        let name = unique_name()?;
        ensure!(
            !self.monitors()?.iter().any(|monitor| monitor.name == name),
            "Headless output name is already in use"
        );
        let mut output = OwnedOutput {
            journal: Journal::create(self, &name)?,
            control: self.clone(),
            name,
            id: None,
            owned: true,
        };
        let reply = self.request(&format!("/output create headless {}", output.name))?;
        if reply != b"ok" {
            output.owned = false;
            output.journal.remove()?;
            bail!(
                "Hyprland rejected headless creation: {}",
                String::from_utf8_lossy(&reply)
            );
        }
        let monitor = find_owned_monitor(&self.monitors()?, &output.name, None)?
            .context("Hyprland did not create the requested named headless output")?;
        output.id = Some(monitor.id);
        output.journal.confirm(monitor.id)?;
        Ok(output)
    }

    fn request(&self, command: &str) -> Result<Vec<u8>> {
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        let metadata = fs::symlink_metadata(&self.path)?;
        ensure!(
            metadata.dev() == self.device && metadata.ino() == self.inode,
            "Hyprland session was replaced"
        );
        let mut stream = socket::connect(&self.path, deadline)?;
        let peer = socket::credentials(&stream)?;
        ensure!(
            peer.uid == self.uid && peer.pid == self.pid,
            "Hyprland control peer changed"
        );
        exchange(&mut stream, command.as_bytes(), deadline)
            .with_context(|| format!("Hyprland control request {command:?} failed"))
    }
}

pub struct OwnedOutput {
    journal: Journal,
    control: Control,
    name: String,
    id: Option<u64>,
    owned: bool,
}

impl OwnedOutput {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn monitor(&self) -> Result<Monitor> {
        find_owned_monitor(&self.control.monitors()?, &self.name, self.id)?
            .context("Owned Hyprland output disappeared")
    }

    pub fn close(&mut self) -> Result<()> {
        if !self.owned {
            return Ok(());
        }
        if find_owned_monitor(&self.control.monitors()?, &self.name, self.id)?.is_some() {
            let reply = self
                .control
                .request(&format!("/output remove {}", self.name))?;
            ensure!(
                reply == b"ok",
                "Hyprland rejected owned-output removal: {}",
                String::from_utf8_lossy(&reply)
            );
        }
        self.owned = false;
        self.journal.remove()?;
        Ok(())
    }
}

impl Drop for OwnedOutput {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            tracing::warn!(
                "Could not remove owned Hyprland output {}: {error:#}",
                self.name
            );
        }
    }
}

fn control_path(runtime: &Path, signature: &str) -> Result<PathBuf> {
    ensure!(
        runtime.is_absolute(),
        "Graphical runtime directory must be absolute"
    );
    ensure!(
        !signature.is_empty()
            && signature.len() <= 128
            && signature
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
        "Invalid Hyprland session signature"
    );
    Ok(runtime.join("hypr").join(signature).join(".socket.sock"))
}

fn validate_version(version: &serde_json::Value) -> Result<()> {
    let supported = ["version", "tag"].iter().any(|key| {
        let Some(version) = version.get(key).and_then(serde_json::Value::as_str) else {
            return false;
        };
        let mut parts = version.trim_start_matches('v').split('.');
        let major = parts.next().and_then(|part| part.parse::<u32>().ok());
        let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
        matches!((major, minor), (Some(major), Some(minor)) if (major, minor) >= (0, 47))
    });
    ensure!(
        supported,
        "Native headless output requires an identifiable Hyprland 0.47 or newer"
    );
    Ok(())
}

fn unique_name() -> Result<String> {
    let mut random = [0; 12];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut name = String::from("LianLi-");
    for byte in random {
        use std::fmt::Write;
        write!(name, "{byte:02x}")?;
    }
    Ok(name)
}

fn find_owned_monitor(
    monitors: &[Monitor],
    name: &str,
    id: Option<u64>,
) -> Result<Option<Monitor>> {
    let mut matching = monitors.iter().filter(|monitor| monitor.name == name);
    let monitor = matching.next();
    ensure!(
        matching.next().is_none(),
        "Hyprland reported duplicate output names"
    );
    if let Some(monitor) = monitor {
        ensure!(
            id.is_none_or(|id| id == monitor.id),
            "Hyprland output identity changed. It will not be used or removed."
        );
    }
    Ok(monitor.cloned())
}

fn exchange(stream: &mut UnixStream, mut command: &[u8], deadline: Instant) -> Result<Vec<u8>> {
    while !command.is_empty() {
        ensure!(
            Instant::now() < deadline,
            "Hyprland control write timed out"
        );
        match stream.write(command) {
            Ok(0) => bail!("Hyprland closed the control connection"),
            Ok(size) => command = &command[size..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                socket::wait(stream.as_raw_fd(), libc::POLLOUT, deadline)?
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut reply = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        ensure!(Instant::now() < deadline, "Hyprland control read timed out");
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(reply),
            Ok(size) => {
                ensure!(
                    reply.len() + size <= MAX_REPLY_BYTES,
                    "Hyprland control reply exceeds 512 KiB"
                );
                reply.extend_from_slice(&buffer[..size]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                socket::wait(stream.as_raw_fd(), libc::POLLIN, deadline)?
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn initialization_sends_version_on_the_first_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            for (expected, reply) in [
                ("j/version", r#"{"version":"0.54.0"}"#),
                ("j/monitors all", "[]"),
            ] {
                socket::wait(
                    listener.as_raw_fd(),
                    libc::POLLIN,
                    Instant::now() + Duration::from_secs(3),
                )
                .unwrap();
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut command = vec![0; expected.len()];
                stream.read_exact(&mut command).unwrap();
                assert_eq!(command, expected.as_bytes());
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let control = Control::connect(path).unwrap();
        assert_eq!(control.uid, unsafe { libc::geteuid() });
        assert_eq!(control.pid, unsafe { libc::getpid() });
        assert!(control.monitors().unwrap().is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn rejects_session_traversal_and_ambiguous_or_replaced_outputs() {
        for signature in ["", "../another", "a/b", "a\nb", "a;exit"] {
            assert!(control_path(Path::new("/run/user/1000"), signature).is_err());
        }
        assert_eq!(
            control_path(Path::new("/run/user/1000"), "hash_123_456").unwrap(),
            Path::new("/run/user/1000/hypr/hash_123_456/.socket.sock")
        );
        let monitor = Monitor {
            id: 3,
            name: "LianLi-test".into(),
            width: 480,
            height: 480,
            refresh_hz: 60.0,
            disabled: false,
        };
        assert!(
            find_owned_monitor(std::slice::from_ref(&monitor), "LianLi-test", Some(2)).is_err()
        );
        assert!(
            find_owned_monitor(&[monitor.clone(), monitor.clone()], "LianLi-test", Some(3))
                .is_err()
        );
        assert!(find_owned_monitor(&[monitor], "unrelated", None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn requires_a_version_with_named_headless_output_support() {
        for version in [
            serde_json::json!({"version": "0.55.0-dev"}),
            serde_json::json!({"tag": "v0.47.0"}),
        ] {
            assert!(validate_version(&version).is_ok());
        }
        for version in [
            serde_json::json!({"tag": "v0.30.0"}),
            serde_json::json!({"commit": "unknown"}),
        ] {
            assert!(validate_version(&version).is_err());
        }
    }

    #[test]
    fn control_exchange_bounds_size_and_total_lifetime() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let mut command = [0; 9];
            server.read_exact(&mut command).unwrap();
            assert_eq!(&command, b"j/version");
            let _ = server.write_all(&vec![0; MAX_REPLY_BYTES + 1]);
        });
        let result = exchange(
            &mut client,
            b"j/version",
            Instant::now() + Duration::from_secs(2),
        );
        assert!(result.unwrap_err().to_string().contains("exceeds"));
        drop(client);
        worker.join().unwrap();

        let (mut client, _server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        let started = Instant::now();
        assert!(exchange(
            &mut client,
            b"j/version",
            started + Duration::from_millis(50)
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
