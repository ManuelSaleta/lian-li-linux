use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use socket2::{Domain, SockAddr, Socket, Type};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const HEADER_BYTES: usize = 12;
const MAX_PACKET_BYTES: usize = 32 * 1024;
const MAX_DESCRIPTORS: usize = 4;
const VERSION: u16 = 1;

pub struct PacketChannel {
    socket: Socket,
    receive_buffer: Vec<u8>,
}

pub struct Received<T> {
    pub message: T,
    pub descriptors: Vec<OwnedFd>,
}

impl PacketChannel {
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self> {
        let socket = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
        socket.set_nonblocking(true)?;
        match socket.connect(&SockAddr::unix(path)?) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
                crate::socket::wait(socket.as_raw_fd(), libc::POLLOUT, Instant::now() + timeout)?;
                if let Some(error) = socket.take_error()? {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error).context("Connecting session display channel"),
        }
        Self::new(socket.into())
    }

    pub fn pair() -> Result<(Self, Self)> {
        let (first, second) = Socket::pair(Domain::UNIX, Type::SEQPACKET, None)?;
        Ok((Self::new(first.into())?, Self::new(second.into())?))
    }

    pub fn new(descriptor: OwnedFd) -> Result<Self> {
        let socket = Socket::from(descriptor);
        ensure!(
            socket.domain()? == Domain::UNIX,
            "Display channel requires a local Unix socket"
        );
        ensure!(
            socket.r#type()? == Type::SEQPACKET,
            "Display channel requires packet boundaries"
        );
        socket.set_nonblocking(true)?;
        socket.set_cloexec(true)?;
        Ok(Self {
            socket,
            receive_buffer: vec![0; MAX_PACKET_BYTES],
        })
    }

    pub fn peer_credentials(&self) -> Result<(u32, i32)> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
        // The owned socket and correctly sized ucred remain valid through getsockopt.
        let result = unsafe {
            libc::getsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut size,
            )
        };
        ensure!(
            result == 0 && size as usize == std::mem::size_of_val(&credentials),
            "Could not verify display peer credentials"
        );
        Ok((credentials.uid, credentials.pid))
    }

    pub fn send<T: Serialize>(
        &self,
        message: &T,
        descriptors: &[BorrowedFd<'_>],
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            descriptors.len() <= MAX_DESCRIPTORS,
            "Too many display descriptors"
        );
        let mut packet = BoundedPacket(vec![0; HEADER_BYTES]);
        serde_json::to_writer(&mut packet, message)
            .context("Serializing display control message")?;
        let size = packet.0.len() - HEADER_BYTES;
        packet.0[..4].copy_from_slice(b"LLDS");
        packet.0[4..6].copy_from_slice(&VERSION.to_le_bytes());
        packet.0[6..8].copy_from_slice(&(descriptors.len() as u16).to_le_bytes());
        packet.0[8..12].copy_from_slice(&(size as u32).to_le_bytes());

        let mut control = [0usize; 8];
        let mut vector = libc::iovec {
            iov_base: packet.0.as_mut_ptr().cast(),
            iov_len: packet.0.len(),
        };
        // msghdr's unused pointer fields must be null for sendmsg.
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut vector;
        header.msg_iovlen = 1;
        if !descriptors.is_empty() {
            header.msg_control = control.as_mut_ptr().cast();
            let data_bytes = descriptors.len() * std::mem::size_of::<libc::c_int>();
            // The aligned control array has room for the header and all four descriptors.
            unsafe {
                header.msg_controllen = libc::CMSG_SPACE(data_bytes as u32) as usize;
                let ancillary = libc::CMSG_FIRSTHDR(&header);
                (*ancillary).cmsg_level = libc::SOL_SOCKET;
                (*ancillary).cmsg_type = libc::SCM_RIGHTS;
                (*ancillary).cmsg_len = libc::CMSG_LEN(data_bytes as u32) as usize;
                let data = libc::CMSG_DATA(ancillary).cast::<libc::c_int>();
                for (index, descriptor) in descriptors.iter().enumerate() {
                    data.add(index).write(descriptor.as_raw_fd());
                }
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            check_deadline(deadline, cancel)?;
            // Packet and ancillary storage remain owned and unchanged until sendmsg returns.
            let result = unsafe {
                libc::sendmsg(
                    self.socket.as_raw_fd(),
                    &header,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if result >= 0 {
                ensure!(
                    result as usize == packet.0.len(),
                    "Display control packet was truncated during send"
                );
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => self.wait(libc::POLLOUT, deadline, cancel)?,
                io::ErrorKind::Interrupted => {}
                _ => return Err(error).context("Sending display control packet"),
            }
        }
    }

    pub fn receive<T: DeserializeOwned>(
        &mut self,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Received<T>> {
        let deadline = Instant::now() + timeout;
        loop {
            check_deadline(deadline, cancel)?;
            if let Some(packet) = self.try_receive()? {
                return Ok(packet);
            }
            self.wait(libc::POLLIN, deadline, cancel)?;
        }
    }

    pub fn try_receive<T: DeserializeOwned>(&mut self) -> Result<Option<Received<T>>> {
        for _ in 0..8 {
            let mut control = [0usize; 8];
            let mut vector = libc::iovec {
                iov_base: self.receive_buffer.as_mut_ptr().cast(),
                iov_len: self.receive_buffer.len(),
            };
            // recvmsg fills only the pointed-to, owned payload and ancillary buffers.
            let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
            header.msg_iov = &mut vector;
            header.msg_iovlen = 1;
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen = std::mem::size_of_val(&control);
            let result = unsafe {
                libc::recvmsg(
                    self.socket.as_raw_fd(),
                    &mut header,
                    libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
                )
            };
            if result >= 0 {
                let mut descriptors = Vec::new();
                // Ancillary records come from the kernel. Adopt every received fd before
                // validating the packet so errors and truncation close them automatically.
                unsafe {
                    let mut ancillary = libc::CMSG_FIRSTHDR(&header);
                    while !ancillary.is_null() {
                        if (*ancillary).cmsg_level == libc::SOL_SOCKET
                            && (*ancillary).cmsg_type == libc::SCM_RIGHTS
                        {
                            let bytes = (*ancillary)
                                .cmsg_len
                                .saturating_sub(libc::CMSG_LEN(0) as usize);
                            let count = bytes / std::mem::size_of::<libc::c_int>();
                            let data = libc::CMSG_DATA(ancillary).cast::<libc::c_int>();
                            for index in 0..count {
                                descriptors
                                    .push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                            }
                        }
                        ancillary = libc::CMSG_NXTHDR(&header, ancillary);
                    }
                }
                ensure!(result > 0, "Display peer disconnected");
                ensure!(
                    header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0,
                    "Display packet or descriptor list was truncated"
                );
                ensure!(
                    descriptors.len() <= MAX_DESCRIPTORS,
                    "Too many received display descriptors"
                );
                let packet = &self.receive_buffer[..result as usize];
                let payload = validate_packet(packet, descriptors.len())?;
                return Ok(Some(Received {
                    message: serde_json::from_slice(payload)
                        .context("Invalid display control payload")?,
                    descriptors,
                }));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => return Ok(None),
                io::ErrorKind::Interrupted => {}
                _ => return Err(error).context("Receiving display control packet"),
            }
        }
        Ok(None)
    }

    fn wait(&self, events: libc::c_short, deadline: Instant, cancel: &AtomicBool) -> Result<()> {
        loop {
            check_deadline(deadline, cancel)?;
            let mut poll = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events,
                revents: 0,
            };
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 50) as i32;
            // The channel owns the descriptor across this bounded cancellation-aware wait.
            let result = unsafe { libc::poll(&mut poll, 1, timeout) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
    }
}

pub fn display_socket_path(ipc_path: &Path) -> PathBuf {
    let mut path = ipc_path.as_os_str().to_os_string();
    path.push(".display");
    PathBuf::from(path)
}

pub struct PacketListener {
    socket: Socket,
    path: PathBuf,
    identity: (u64, u64),
}

impl PacketListener {
    pub fn bind(path: PathBuf) -> Result<Self> {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket()
                        && metadata.uid() == unsafe { libc::geteuid() },
                    "Display socket path belongs to another owner or is not a socket"
                );
                let error = PacketChannel::connect(&path, Duration::from_millis(200))
                    .err()
                    .context("Display socket is already in use")?;
                ensure!(
                    error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::ConnectionRefused),
                    "Cannot establish that the previous display socket is unused"
                );
                std::fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let socket = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
        socket.set_nonblocking(true)?;
        socket.set_cloexec(true)?;
        socket.bind(&SockAddr::unix(&path)?)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let listener = Self {
            socket,
            path,
            identity: (metadata.dev(), metadata.ino()),
        };
        std::fs::set_permissions(&listener.path, std::fs::Permissions::from_mode(0o666))?;
        listener.socket.listen(8)?;
        Ok(listener)
    }

    pub fn accept(&self) -> Result<Option<PacketChannel>> {
        match self.socket.accept() {
            Ok((socket, _)) => Ok(Some(PacketChannel::new(socket.into())?)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl AsFd for PacketListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl Drop for PacketListener {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity)
        {
            if let Err(error) = std::fs::remove_file(&self.path) {
                tracing::debug!("Removing owned display socket failed: {error}");
            }
        }
    }
}

impl AsFd for PacketChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

fn check_deadline(deadline: Instant, cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Display channel operation cancelled"
    );
    if Instant::now() >= deadline {
        return Err(io::Error::from(io::ErrorKind::TimedOut).into());
    }
    Ok(())
}

fn validate_packet(packet: &[u8], descriptor_count: usize) -> Result<&[u8]> {
    ensure!(
        packet.len() >= HEADER_BYTES && packet[..4] == *b"LLDS",
        "Invalid display channel header"
    );
    let version = u16::from_le_bytes([packet[4], packet[5]]);
    ensure!(
        version == VERSION,
        "Incompatible display channel version {version}"
    );
    let descriptors = u16::from_le_bytes([packet[6], packet[7]]) as usize;
    ensure!(
        descriptors == descriptor_count,
        "Display descriptor count mismatch"
    );
    let size = u32::from_le_bytes(packet[8..12].try_into().unwrap()) as usize;
    ensure!(
        size == packet.len() - HEADER_BYTES,
        "Display packet size mismatch"
    );
    Ok(&packet[HEADER_BYTES..])
}

struct BoundedPacket(Vec<u8>);

impl Write for BoundedPacket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_PACKET_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Display control message exceeds 32 KiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::FileExt;

    #[test]
    fn listener_preserves_non_socket_paths_and_authenticates_connected_peers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display");
        std::fs::write(&path, b"keep").unwrap();
        assert!(PacketListener::bind(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_file(&path).unwrap();
        let listener = PacketListener::bind(path.clone()).unwrap();
        let client = PacketChannel::connect(&path, Duration::from_secs(1)).unwrap();
        let mut server = listener.accept().unwrap().unwrap();
        assert_eq!(server.peer_credentials().unwrap().0, unsafe {
            libc::geteuid()
        });
        assert!(server.try_receive::<String>().unwrap().is_none());
        client
            .send(
                &"ready",
                &[],
                Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            server.try_receive::<String>().unwrap().unwrap().message,
            "ready"
        );
        assert!(PacketListener::bind(path.clone()).is_err());
        drop(listener);
        assert!(!path.exists());
    }

    #[test]
    fn sends_bounded_control_and_owned_close_on_exec_descriptors() {
        let (sender, mut receiver) = PacketChannel::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        file.write_all_at(b"frame bytes", 0).unwrap();
        let cancel = AtomicBool::new(false);
        sender
            .send(
                &serde_json::json!({"sequence": 42}),
                &[file.as_fd()],
                Duration::from_secs(1),
                &cancel,
            )
            .unwrap();
        let mut received: Received<serde_json::Value> =
            receiver.receive(Duration::from_secs(1), &cancel).unwrap();
        assert_eq!(received.message["sequence"], 42);
        assert_eq!(received.descriptors.len(), 1);
        let descriptor = received.descriptors.pop().unwrap();
        assert_ne!(
            unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut bytes = [0; 11];
        File::from(descriptor).read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"frame bytes");
        assert_eq!(
            receiver.peer_credentials().unwrap(),
            (unsafe { libc::geteuid() }, std::process::id() as i32)
        );
        assert!(sender
            .send(
                &"x".repeat(MAX_PACKET_BYTES),
                &[],
                Duration::from_secs(1),
                &cancel
            )
            .is_err());
    }

    #[test]
    fn rejects_wrong_version_truncated_payload_and_missing_descriptors() {
        let mut packet = b"LLDS\x01\x00\x00\x00\x02\x00\x00\x00{}".to_vec();
        assert_eq!(validate_packet(&packet, 0).unwrap(), b"{}");
        assert!(validate_packet(&packet, 1).is_err());
        packet[4] = 2;
        assert!(validate_packet(&packet, 0).is_err());
        packet[4] = 1;
        packet.pop();
        assert!(validate_packet(&packet, 0).is_err());
    }

    #[test]
    fn idle_channel_waits_are_cancellable_and_bounded() {
        let (_sender, mut receiver) = PacketChannel::pair().unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let worker = std::thread::spawn(move || {
            receiver.receive::<serde_json::Value>(Duration::from_secs(30), &stop)
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn full_queue_reports_failure_and_preserves_accepted_packet_order() {
        let (sender, mut receiver) = PacketChannel::pair().unwrap();
        sender.socket.set_send_buffer_size(32768).unwrap();
        let cancel = AtomicBool::new(false);
        let mut accepted = 0;
        for sequence in 0..64 {
            let message = serde_json::json!({"sequence": sequence, "data": "x".repeat(16384)});
            if sender
                .send(&message, &[], Duration::from_millis(20), &cancel)
                .is_err()
            {
                break;
            }
            accepted += 1;
        }
        assert!(accepted > 0 && accepted < 64);
        for sequence in 0..accepted {
            let received: Received<serde_json::Value> =
                receiver.receive(Duration::from_secs(1), &cancel).unwrap();
            assert_eq!(received.message["sequence"], sequence);
        }
        sender
            .send(&"recovered", &[], Duration::from_secs(1), &cancel)
            .unwrap();
        let received: Received<String> = receiver.receive(Duration::from_secs(1), &cancel).unwrap();
        assert_eq!(received.message, "recovered");
    }
}
