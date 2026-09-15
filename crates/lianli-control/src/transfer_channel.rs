use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use socket2::{Domain, Socket, Type};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const MAX_PACKET: usize = 32 * 1024;

pub(crate) struct Channel {
    socket: Socket,
    deadline: Instant,
}

impl Channel {
    pub fn pair() -> Result<(OwnedFd, OwnedFd)> {
        let (left, right) = Socket::pair(Domain::UNIX, Type::SEQPACKET, None)?;
        left.set_cloexec(true)?;
        right.set_cloexec(true)?;
        Ok((left.into(), right.into()))
    }

    pub fn new(fd: OwnedFd, timeout: Duration) -> Result<Self> {
        let socket = Socket::from(fd);
        ensure!(
            socket.domain()? == Domain::UNIX && socket.r#type()? == Type::SEQPACKET,
            "State transfer requires an inherited Unix packet channel"
        );
        socket.set_nonblocking(true)?;
        socket.set_cloexec(true)?;
        Ok(Self {
            socket,
            deadline: Instant::now() + timeout.min(Duration::from_secs(600)),
        })
    }

    pub fn send<T: Serialize>(&self, value: &T, fd: Option<BorrowedFd<'_>>) -> Result<()> {
        let mut packet = Packet(b"LLST\x01\0".to_vec());
        serde_json::to_writer(&mut packet, value)?;
        let mut vector = libc::iovec {
            iov_base: packet.0.as_mut_ptr().cast(),
            iov_len: packet.0.len(),
        };
        let mut ancillary = [0usize; 8];
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut vector;
        header.msg_iovlen = 1;
        if let Some(fd) = fd {
            header.msg_control = ancillary.as_mut_ptr().cast();
            unsafe {
                header.msg_controllen =
                    libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as usize;
                let control = libc::CMSG_FIRSTHDR(&header);
                (*control).cmsg_level = libc::SOL_SOCKET;
                (*control).cmsg_type = libc::SCM_RIGHTS;
                (*control).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
                libc::CMSG_DATA(control).cast::<i32>().write(fd.as_raw_fd());
            }
        }
        loop {
            self.check()?;
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
                    "State transfer packet was truncated"
                );
                return Ok(());
            }
            match io::Error::last_os_error().kind() {
                io::ErrorKind::WouldBlock => self.wait(libc::POLLOUT)?,
                io::ErrorKind::Interrupted => {}
                _ => {
                    return Err(io::Error::last_os_error()).context("Sending state transfer packet")
                }
            }
        }
    }

    pub fn receive<T: DeserializeOwned>(&self) -> Result<(T, Option<OwnedFd>)> {
        let mut bytes = vec![0u8; MAX_PACKET];
        loop {
            self.check()?;
            let mut vector = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            };
            let mut ancillary = [0usize; 8];
            let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
            header.msg_iov = &mut vector;
            header.msg_iovlen = 1;
            header.msg_control = ancillary.as_mut_ptr().cast();
            header.msg_controllen = std::mem::size_of_val(&ancillary);
            let result = unsafe {
                libc::recvmsg(
                    self.socket.as_raw_fd(),
                    &mut header,
                    libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
                )
            };
            if result >= 0 {
                let mut descriptors = Vec::new();
                // Adopt all kernel-delivered descriptors before validating, including truncated packets.
                unsafe {
                    let mut control = libc::CMSG_FIRSTHDR(&header);
                    while !control.is_null() {
                        if (*control).cmsg_level == libc::SOL_SOCKET
                            && (*control).cmsg_type == libc::SCM_RIGHTS
                        {
                            let count = ((*control).cmsg_len - libc::CMSG_LEN(0) as usize)
                                / std::mem::size_of::<i32>();
                            for index in 0..count {
                                descriptors.push(OwnedFd::from_raw_fd(
                                    libc::CMSG_DATA(control)
                                        .cast::<i32>()
                                        .add(index)
                                        .read_unaligned(),
                                ));
                            }
                        }
                        control = libc::CMSG_NXTHDR(&header, control);
                    }
                }
                ensure!(result > 0, "State transfer peer disconnected");
                ensure!(
                    header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
                        && descriptors.len() <= 1,
                    "State transfer packet or descriptors exceed their limit"
                );
                let packet = &bytes[..result as usize];
                ensure!(
                    packet.starts_with(b"LLST\x01\0"),
                    "Unsupported state transfer protocol"
                );
                return Ok((serde_json::from_slice(&packet[6..])?, descriptors.pop()));
            }
            match io::Error::last_os_error().kind() {
                io::ErrorKind::WouldBlock => self.wait(libc::POLLIN)?,
                io::ErrorKind::Interrupted => {}
                _ => {
                    return Err(io::Error::last_os_error())
                        .context("Receiving state transfer packet")
                }
            }
        }
    }

    fn check(&self) -> Result<()> {
        ensure!(Instant::now() < self.deadline, "State transfer timed out");
        Ok(())
    }

    fn wait(&self, events: i16) -> Result<()> {
        loop {
            self.check()?;
            let mut descriptor = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events,
                revents: 0,
            };
            let timeout = self
                .deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error()).context("Waiting for state transfer");
            }
        }
    }
}

struct Packet(Vec<u8>);

impl Write for Packet {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_PACKET.saturating_sub(self.0.len()) {
            return Err(io::Error::other("State transfer metadata exceeds 32 KiB"));
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
    use std::io::Read;
    use std::os::fd::AsFd;

    #[test]
    fn passes_owned_descriptors_with_cloexec_and_preserves_packet_boundaries() {
        let (left, right) = Channel::pair().unwrap();
        let sender = Channel::new(left, Duration::from_secs(1)).unwrap();
        let receiver = Channel::new(right, Duration::from_secs(1)).unwrap();
        let file = tempfile::tempfile().unwrap();
        sender.send(&"first", Some(file.as_fd())).unwrap();
        sender.send(&"second", None).unwrap();
        let (message, descriptor): (String, _) = receiver.receive().unwrap();
        assert_eq!(message, "first");
        let descriptor = descriptor.unwrap();
        assert_ne!(
            unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let (message, descriptor): (String, _) = receiver.receive().unwrap();
        assert_eq!(message, "second");
        assert!(descriptor.is_none());
    }

    #[test]
    fn failed_decode_closes_the_delivered_descriptor() {
        let (left, right) = Channel::pair().unwrap();
        let sender = Channel::new(left, Duration::from_secs(1)).unwrap();
        let receiver = Channel::new(right, Duration::from_secs(1)).unwrap();
        let (reader, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        sender.send(&17, Some(reader.as_fd())).unwrap();
        drop(reader);
        assert!(receiver.receive::<String>().is_err());
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn oversized_metadata_disconnect_and_idle_peer_are_bounded() {
        let (left, right) = Channel::pair().unwrap();
        let sender = Channel::new(left, Duration::from_secs(1)).unwrap();
        let receiver = Channel::new(right, Duration::from_millis(5)).unwrap();
        assert!(sender.send(&"x".repeat(MAX_PACKET), None).is_err());
        assert!(receiver
            .receive::<String>()
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        // Shutdown also disconnects descriptors inherited by parallel fork-to-exec fixtures.
        let _inherited = receiver.socket.try_clone().unwrap();
        receiver.socket.shutdown(std::net::Shutdown::Both).unwrap();
        drop(receiver);
        assert!(sender.send(&"closed", None).is_err());
    }
}
