use anyhow::{bail, Context, Result};
use socket2::{Domain, SockAddr, Socket, Type};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

pub(crate) fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    let address = SockAddr::unix(path)?;
    match socket.connect(&address) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
            wait(socket.as_raw_fd(), libc::POLLOUT, deadline)?;
            if let Some(error) = socket.take_error()? {
                return Err(error.into());
            }
        }
        Err(error) => return Err(error).context("connecting display control socket"),
    }
    let descriptor: std::os::fd::OwnedFd = socket.into();
    Ok(descriptor.into())
}

pub(crate) fn wait(fd: libc::c_int, events: libc::c_short, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("Display control request timed out");
        }
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // The caller owns the descriptor until this bounded wait completes.
        let result = unsafe { libc::poll(&mut poll, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if result > 0 {
            return Ok(());
        }
    }
}

pub(crate) fn credentials(stream: &UnixStream) -> Result<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
    // SO_PEERCRED fills this correctly sized ucred while the stream owns its fd.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error().into());
    }
    anyhow::ensure!(
        size as usize == std::mem::size_of_val(&credentials),
        "Invalid display peer credentials"
    );
    Ok(credentials)
}
