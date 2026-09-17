//! Bounded execution for external ffmpeg/ffprobe helpers.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Deadline for a one-shot transcode (a still image or short clip to H.264).
/// Generous: large sources on a slow CPU are legitimately slow.
pub(crate) const ENCODE_TIMEOUT: Duration = Duration::from_secs(120);

/// Deadline for a metadata probe, which reads headers and should return
/// almost immediately.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STDOUT_LIMIT: usize = 16 * 1024 * 1024;
const STDERR_LIMIT: usize = 256 * 1024;

#[cfg(test)]
pub(crate) fn output_with_timeout(cmd: Command, timeout: Duration) -> Result<Output, TimedOut> {
    output_cancellable(cmd, timeout, &AtomicBool::new(false))
}

pub(crate) fn output_cancellable(
    mut cmd: Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output, TimedOut> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    if cancel.load(Ordering::Relaxed) {
        return Err(TimedOut::Cancelled { program });
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let child = cmd.spawn().map_err(|source| TimedOut::Spawn {
        program: program.clone(),
        source,
    })?;
    capture_output(
        &mut Helper {
            child,
            reaped: false,
        },
        &program,
        timeout,
        cancel,
    )
}

fn capture_output(
    child: &mut Helper,
    program: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output, TimedOut> {
    let io_error = |source| TimedOut::Spawn {
        program: program.into(),
        source,
    };
    let mut stdout =
        PipeCapture::new(child.child.stdout.take().expect("stdout was piped")).map_err(io_error)?;
    let mut stderr =
        PipeCapture::new(child.child.stderr.take().expect("stderr was piped")).map_err(io_error)?;
    let deadline = Instant::now() + timeout;
    let mut exited = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(TimedOut::Cancelled {
                program: program.into(),
            });
        }
        if Instant::now() >= deadline {
            return Err(TimedOut::Deadline {
                program: program.into(),
                timeout,
            });
        }
        stdout.drain(STDOUT_LIMIT).map_err(io_error)?;
        stderr.drain(STDERR_LIMIT).map_err(io_error)?;
        if !exited {
            exited = child.exited().map_err(io_error)?;
        }
        if exited && stdout.eof && stderr.eof {
            return Ok(Output {
                status: child.finish().map_err(io_error)?,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
            });
        }
        let mut pipes = [stdout.poll_fd(), stderr.poll_fd()];
        let wait = POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        // Both readers own their descriptors throughout poll; EOF descriptors are ignored.
        let result = unsafe {
            libc::poll(
                pipes.as_mut_ptr(),
                pipes.len() as libc::nfds_t,
                wait.as_millis() as libc::c_int,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(io_error(error));
            }
        }
    }
}

struct PipeCapture<R> {
    reader: R,
    bytes: Vec<u8>,
    eof: bool,
}

pub(crate) fn stream_frames(
    command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
    frame_bytes: usize,
    consume: impl FnMut(&[u8]) -> Result<(), crate::MediaError>,
) -> Result<Output, crate::MediaError> {
    read_frames(command, timeout, false, cancel, frame_bytes, consume)
}

pub(crate) fn stream_live_frames(
    command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
    frame_bytes: usize,
    consume: impl FnMut(&[u8]) -> Result<(), crate::MediaError>,
) -> Result<Output, crate::MediaError> {
    read_frames(command, timeout, true, cancel, frame_bytes, consume)
}

fn read_frames(
    mut command: Command,
    timeout: Duration,
    idle_timeout: bool,
    cancel: &AtomicBool,
    frame_bytes: usize,
    mut consume: impl FnMut(&[u8]) -> Result<(), crate::MediaError>,
) -> Result<Output, crate::MediaError> {
    use crate::MediaError;
    if frame_bytes == 0 || frame_bytes > 64 * 1024 * 1024 {
        return Err(MediaError::InvalidConfig(
            "Invalid decoded frame size".into(),
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(MediaError::Cancelled);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut helper = Helper {
        child: command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?,
        reaped: false,
    };
    let mut stdout = PipeCapture::new(helper.child.stdout.take().expect("stdout was piped"))?;
    stdout.bytes = Vec::with_capacity(frame_bytes);
    let mut stderr = PipeCapture::new(helper.child.stderr.take().expect("stderr was piped"))?;
    let mut deadline = Instant::now() + timeout;
    let mut exited = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(MediaError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(TimedOut::Deadline { program, timeout }.into());
        }
        if stdout.drain_frames(frame_bytes, &mut consume)? && idle_timeout {
            deadline = Instant::now() + timeout;
        }
        stderr.drain(STDERR_LIMIT)?;
        if !exited {
            exited = helper.exited()?;
        }
        if exited && stdout.eof && stderr.eof {
            return Ok(Output {
                status: helper.finish()?,
                stdout: Vec::new(),
                stderr: stderr.bytes,
            });
        }
        let mut pipes = [stdout.poll_fd(), stderr.poll_fd()];
        let wait = POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        // The pipe readers retain both descriptors until poll returns.
        let result = unsafe {
            libc::poll(
                pipes.as_mut_ptr(),
                pipes.len() as libc::nfds_t,
                wait.as_millis() as libc::c_int,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
}

impl<R: Read + AsRawFd> PipeCapture<R> {
    fn drain_frames(
        &mut self,
        frame_bytes: usize,
        consume: &mut impl FnMut(&[u8]) -> Result<(), crate::MediaError>,
    ) -> Result<bool, crate::MediaError> {
        if self.eof {
            return Ok(false);
        }
        let mut progressed = false;
        let mut buffer = [0u8; 8192];
        for _ in 0..32 {
            let length = buffer.len().min(frame_bytes - self.bytes.len());
            match self.reader.read(&mut buffer[..length]) {
                Ok(0) => {
                    if !self.bytes.is_empty() {
                        return Err(crate::MediaError::Ffmpeg(
                            "Incomplete decoded RGBA frame".into(),
                        ));
                    }
                    self.eof = true;
                    break;
                }
                Ok(size) => {
                    progressed = true;
                    self.bytes.extend_from_slice(&buffer[..size]);
                    if self.bytes.len() == frame_bytes {
                        consume(&self.bytes)?;
                        self.bytes.clear();
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(progressed)
    }
    fn poll_fd(&self) -> libc::pollfd {
        libc::pollfd {
            fd: if self.eof {
                -1
            } else {
                self.reader.as_raw_fd()
            },
            events: libc::POLLIN,
            revents: 0,
        }
    }

    fn new(reader: R) -> io::Result<Self> {
        let fd = reader.as_raw_fd();
        // The owned reader keeps this pipe descriptor alive through both fcntl calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            reader,
            bytes: Vec::new(),
            eof: false,
        })
    }

    fn drain(&mut self, limit: usize) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0; 8192];
        for _ in 0..32 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(size) => {
                    if size > limit.saturating_sub(self.bytes.len()) {
                        return Err(io::Error::other(format!(
                            "Helper output exceeds {limit} bytes"
                        )));
                    }
                    self.bytes.extend_from_slice(&buffer[..size]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

struct Helper {
    child: Child,
    reaped: bool,
}

impl Helper {
    fn exited(&self) -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    fn finish(&mut self) -> io::Result<ExitStatus> {
        // Retain the leader's PID until group cleanup so it cannot identify another process.
        let pgid = -(self.child.id() as libc::pid_t);
        if unsafe { libc::kill(pgid, libc::SIGKILL) } == -1 {
            let _ = self.child.kill();
        }
        let result = self.child.wait();
        self.reaped = true;
        result
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.finish();
        }
    }
}

impl From<TimedOut> for crate::common::MediaError {
    fn from(err: TimedOut) -> Self {
        match err {
            TimedOut::Cancelled { .. } => Self::Cancelled,
            TimedOut::Spawn { source, .. } => Self::Io(source),
            deadline => Self::HelperTimedOut(deadline.to_string()),
        }
    }
}

/// Why a bounded run did not produce an exit status.
#[derive(Debug)]
pub(crate) enum TimedOut {
    Cancelled {
        program: String,
    },
    /// Starting, waiting or reading the helper failed.
    Spawn {
        program: String,
        source: std::io::Error,
    },
    /// The process outlived its deadline and was killed.
    Deadline {
        program: String,
        timeout: Duration,
    },
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled { program } => write!(f, "{program} was cancelled"),
            Self::Spawn { program, source } => write!(f, "could not complete {program}: {source}"),
            Self::Deadline { program, timeout } => write!(
                f,
                "{program} did not exit within {}s and was killed",
                timeout.as_secs()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_stream_timeout_excludes_consumer_backpressure() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf abcdefgh"]);
        let mut frames = Vec::new();
        let output = stream_live_frames(
            command,
            Duration::from_secs(1),
            &AtomicBool::new(false),
            4,
            |frame| {
                frames.push(frame.to_vec());
                std::thread::sleep(Duration::from_millis(1100));
                Ok(())
            },
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(frames, vec![b"abcd".to_vec(), b"efgh".to_vec()]);
    }

    #[test]
    fn streamed_frames_preserve_boundaries_and_reject_partial_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf ab; printf cdef; printf gh"]);
        let mut frames = Vec::new();
        let output = stream_frames(
            command,
            Duration::from_secs(3),
            &AtomicBool::new(false),
            4,
            |frame| {
                frames.push(frame.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(frames, vec![b"abcd".to_vec(), b"efgh".to_vec()]);
        assert!(output.stdout.is_empty());
        let mut command = Command::new("sh");
        command.args(["-c", "printf abc"]);
        assert!(
            matches!(stream_frames(command, Duration::from_secs(3), &AtomicBool::new(false), 4, |_| Ok(())), Err(crate::MediaError::Ffmpeg(message)) if message.contains("Incomplete"))
        );
    }

    #[test]
    fn stream_consumer_failure_stops_the_producer_and_preserves_the_error() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf abcd; sleep 10"]);
        let started = Instant::now();
        let result = stream_frames(
            command,
            Duration::from_secs(15),
            &AtomicBool::new(false),
            4,
            |_| Err(crate::MediaError::InvalidConfig("retained budget".into())),
        );
        assert!(
            matches!(result, Err(crate::MediaError::InvalidConfig(message)) if message == "retained budget")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(
            stream_frames(
                Command::new("missing-private-fixture"),
                Duration::from_secs(1),
                &AtomicBool::new(true),
                4,
                |_| Ok(())
            ),
            Err(crate::MediaError::Cancelled)
        ));
    }

    #[test]
    fn frame_stream_handles_large_output_without_retaining_stdout() {
        let mut command = Command::new("head");
        command.args(["-c", "25165824", "/dev/zero"]);
        let mut frames = 0;
        let output = stream_frames(
            command,
            Duration::from_secs(10),
            &AtomicBool::new(false),
            8 * 1024 * 1024,
            |frame| {
                assert_eq!(frame.len(), 8 * 1024 * 1024);
                frames += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(frames, 3);
    }

    #[test]
    fn frame_stream_honors_timeout_and_cancellation_after_a_frame() {
        let mut command = Command::new("sleep");
        command.arg("10");
        assert!(matches!(
            stream_frames(
                command,
                Duration::from_millis(50),
                &AtomicBool::new(false),
                4,
                |_| Ok(())
            ),
            Err(crate::MediaError::HelperTimedOut(_))
        ));
        let cancel = AtomicBool::new(false);
        let mut command = Command::new("sh");
        command.args(["-c", "printf abcd; sleep 10"]);
        assert!(matches!(
            stream_frames(command, Duration::from_secs(3), &cancel, 4, |_| {
                cancel.store(true, Ordering::Relaxed);
                Ok(())
            }),
            Err(crate::MediaError::Cancelled)
        ));
    }

    #[test]
    fn exited_leader_retains_its_process_group_identity_until_cleanup() {
        let child = Command::new("sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut helper = Helper {
            child,
            reaped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !helper.exited().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid = helper.child.id() as libc::pid_t;
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert!(helper.exited().unwrap());
        assert_eq!(helper.finish().unwrap().code(), Some(7));
    }

    #[test]
    fn captures_a_large_frame_through_a_small_pipe_without_throttling() {
        let mut command = Command::new("head");
        command
            .args(["-c", "8388608", "/dev/zero"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // Resize after stdio setup but before the writer can fill the pipe.
        unsafe {
            command.pre_exec(|| {
                if libc::fcntl(libc::STDOUT_FILENO, libc::F_SETPIPE_SZ, 4096) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = Helper {
            child: command.spawn().unwrap(),
            reaped: false,
        };
        let result = capture_output(
            &mut child,
            "head",
            Duration::from_secs(5),
            &AtomicBool::new(false),
        );
        let output = result.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, vec![0; 8 * 1024 * 1024]);
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn deadline_covers_pipes_inherited_by_a_descendant_after_parent_exit() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let started = Instant::now();
        let result = output_with_timeout(command, Duration::from_millis(200));
        assert!(matches!(result, Err(TimedOut::Deadline { .. })));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn excessive_output_fails_without_retaining_unbounded_memory() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 300000 /dev/zero >&2"]);
        let result = output_with_timeout(command, Duration::from_secs(5));
        assert!(
            matches!(result, Err(TimedOut::Spawn { source, .. }) if source.to_string().contains("output exceeds"))
        );
    }

    #[test]
    fn cancellation_reaps_a_running_helper() {
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let worker = std::thread::spawn(move || {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30"]);
            output_cancellable(command, Duration::from_secs(10), &cancel)
        });
        std::thread::sleep(Duration::from_millis(100));
        cancelled.store(true, Ordering::Relaxed);
        assert!(matches!(
            worker.join().unwrap(),
            Err(TimedOut::Cancelled { .. })
        ));
    }

    /// A child that exits normally still yields its output.
    #[test]
    fn returns_output_of_a_command_that_exits() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf out; printf err >&2; exit 3"]);

        let out = output_with_timeout(cmd, Duration::from_secs(30)).expect("should not time out");

        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
    }

    /// The regression this module exists for: a child that never exits must not
    /// block the caller forever.
    #[test]
    fn kills_a_child_that_never_exits() {
        let mut cmd = Command::new("sleep");
        cmd.arg("600");

        let started = Instant::now();
        let err = output_with_timeout(cmd, Duration::from_millis(300))
            .expect_err("a child that outlives the deadline must be reported");

        assert!(
            matches!(err, TimedOut::Deadline { .. }),
            "expected a deadline error, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "returned after {:?}; it should return at the deadline, not wait for the child",
            started.elapsed()
        );
    }

    /// A child that outlives the deadline while holding its pipes open (the
    /// shape of the hung ffmpeg) is still cut off at the deadline.
    #[test]
    fn kills_a_child_that_holds_its_pipes_open() {
        let mut cmd = Command::new("sh");
        // Writes, then hangs without closing stdout - so waiting for EOF on the
        // pipes, as `Command::output()` does, would never return.
        cmd.args(["-c", "printf partial; sleep 600"]);

        let started = Instant::now();
        let err = output_with_timeout(cmd, Duration::from_millis(300))
            .expect_err("a child holding its pipes open must still be cut off");

        assert!(matches!(err, TimedOut::Deadline { .. }), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    /// A missing binary is reported rather than treated as a timeout.
    #[test]
    fn reports_a_command_that_cannot_be_spawned() {
        let cmd = Command::new("lianli-no-such-binary-should-exist");

        let err = output_with_timeout(cmd, Duration::from_secs(30))
            .expect_err("spawning a missing binary must fail");

        assert!(matches!(err, TimedOut::Spawn { .. }), "got {err:?}");
    }
}
