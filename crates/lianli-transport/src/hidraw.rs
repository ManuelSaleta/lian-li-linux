use crate::error::TransportError;
use crate::hid_trait::HidTransport;
use hidapi::HidDevice;
use std::ffi::OsString;
use std::os::unix::io::AsRawFd;
use tracing::{debug, warn};

const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct HidrawTransport {
    device: HidDevice,
    /// hidapi write has no timeout, use a nonblocking fd for writes
    write_fd: Option<std::fs::File>,
}

impl HidrawTransport {
    pub fn new(device: HidDevice, path: Option<OsString>) -> Self {
        let write_fd =
            path.and_then(
                |p| match std::fs::OpenOptions::new().write(true).read(false).open(&p) {
                    Ok(f) => {
                        let fd = f.as_raw_fd();
                        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                        if flags >= 0
                            && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }
                                >= 0
                        {
                            Some(f)
                        } else {
                            warn!(
                                "hidraw: failed to set O_NONBLOCK on {:?}, using hidapi write",
                                p
                            );
                            None
                        }
                    }
                    Err(e) => {
                        debug!(
                            "hidraw: no parallel write fd for {:?}: {e}, using hidapi write",
                            p
                        );
                        None
                    }
                },
            );
        Self { device, write_fd }
    }

    fn write_timed(
        &self,
        data: &[u8],
        timeout: std::time::Duration,
    ) -> Result<usize, TransportError> {
        match self.write_fd {
            Some(ref f) => write_fd_timed(f.as_raw_fd(), data, timeout),
            None => self
                .device
                .write(data)
                .map_err(|e| TransportError::Write(e.to_string())),
        }
    }
}

fn write_fd_timed(
    fd: std::os::unix::io::RawFd,
    data: &[u8],
    timeout: std::time::Duration,
) -> Result<usize, TransportError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut written = 0usize;
    loop {
        let n = unsafe { libc::write(fd, data[written..].as_ptr().cast(), data.len() - written) };
        if n >= 0 {
            written += n as usize;
            if written == data.len() {
                return Ok(written);
            }
            continue;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) => {
                let Some(remain) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    return Err(TransportError::Write(format!(
                        "hidraw write timed out after {written}/{} bytes",
                        data.len()
                    )));
                };
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let r = unsafe {
                    libc::poll(
                        &mut pfd,
                        1,
                        remain.as_millis().clamp(1, i32::MAX as u128) as i32,
                    )
                };
                if r < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(TransportError::Write(e.to_string()));
                }
                if r == 0 {
                    return Err(TransportError::Write(format!(
                        "hidraw write timed out after {written}/{} bytes",
                        data.len()
                    )));
                }
            }
            Some(libc::EINTR) => continue,
            _ => return Err(TransportError::Write(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn write_fd_timed_times_out_on_full_buffer() {
        let (mut tx, _rx) = std::os::unix::net::UnixStream::pair().unwrap();
        tx.set_nonblocking(true).unwrap();

        let chunk = [0u8; 4096];
        let mut filled = 0usize;
        loop {
            match tx.write(&chunk) {
                Ok(n) => filled += n,
                Err(_) => break,
            }
        }
        assert!(filled > 0, "socket buffer never filled?");

        let start = std::time::Instant::now();
        let result = write_fd_timed(
            tx.as_raw_fd(),
            &[0xAA; 64],
            std::time::Duration::from_millis(150),
        );
        let elapsed = start.elapsed();
        assert!(result.is_err(), "expected timeout error");
        assert!(
            elapsed >= std::time::Duration::from_millis(120),
            "returned too fast: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(2_000),
            "blocked far past deadline: {elapsed:?}"
        );
    }

    #[test]
    fn write_fd_timed_writes_when_draining() {
        let (mut tx, mut rx) = std::os::unix::net::UnixStream::pair().unwrap();
        tx.set_nonblocking(true).unwrap();
        rx.set_nonblocking(true).unwrap();

        let data = [0x55u8; 32];
        let n =
            write_fd_timed(tx.as_raw_fd(), &data, std::time::Duration::from_millis(500)).unwrap();
        assert_eq!(n, 32);
    }
}

impl HidTransport for HidrawTransport {
    fn write(&mut self, data: &[u8]) -> Result<usize, TransportError> {
        self.write_timed(data, WRITE_TIMEOUT)
    }

    fn read_timeout(&mut self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, TransportError> {
        self.device
            .read_timeout(buf, timeout_ms)
            .map_err(|e| TransportError::Read(e.to_string()))
    }

    fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError> {
        self.device
            .send_feature_report(data)
            .map_err(|e| TransportError::Write(e.to_string()))?;
        Ok(data.len())
    }

    fn get_feature_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.device
            .get_feature_report(buf)
            .map_err(|e| TransportError::Read(e.to_string()))
    }

    fn get_input_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.device
            .get_input_report(buf)
            .map_err(|e| TransportError::Read(e.to_string()))
    }

    fn read_flush(&mut self) {
        let mut buf = [0u8; 64];
        loop {
            match self.device.read_timeout(&mut buf, 5) {
                Ok(n) if n > 0 => continue,
                _ => break,
            }
        }
    }
}
