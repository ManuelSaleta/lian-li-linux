use anyhow::{bail, ensure, Context, Result};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

struct Process {
    child: Child,
    reaped: bool,
}

impl Process {
    fn exited(&self) -> Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ensure!(
            result == 0,
            "Checking diagnostic process: {}",
            io::Error::last_os_error()
        );
        Ok(unsafe { info.si_pid() } != 0)
    }

    fn finish(&mut self) -> io::Result<ExitStatus> {
        // Keep the leader unreaped until group cleanup so its ID cannot identify another process.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.finish();
        }
    }
}

pub fn run(command: Command, timeout: Duration) -> Result<Output> {
    run_with_stdin(command, Stdio::null(), timeout)
}

pub fn run_with_stdin(command: Command, stdin: Stdio, timeout: Duration) -> Result<Output> {
    run_with_stdin_limit(command, stdin, timeout, MAX_OUTPUT)
}

pub fn run_with_stdin_limit(
    command: Command,
    stdin: Stdio,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<Output> {
    run_inner(command, stdin, timeout, stdout_limit, None)
}

pub(crate) fn run_cancelable(
    command: Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output> {
    run_inner(command, Stdio::null(), timeout, MAX_OUTPUT, Some(cancel))
}

fn run_inner(
    mut command: Command,
    stdin: Stdio,
    timeout: Duration,
    stdout_limit: usize,
    cancel: Option<&AtomicBool>,
) -> Result<Output> {
    ensure!(
        (1..=16 * 1024 * 1024).contains(&stdout_limit),
        "Helper stdout limit must be between 1 byte and 16 MiB"
    );
    command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ECANCELED));
            }
            Ok(())
        });
    }
    let mut child = Process {
        child: command.spawn().context("Starting diagnostic command")?,
        reaped: false,
    };
    drop(command);
    let mut stdout = child
        .child
        .stdout
        .take()
        .context("Missing diagnostic stdout")?;
    let mut stderr = child
        .child
        .stderr
        .take()
        .context("Missing diagnostic stderr")?;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut status = None;
    loop {
        ensure!(
            !cancel.is_some_and(|value| value.load(Ordering::Acquire)),
            "Diagnostic command cancelled"
        );
        ensure!(Instant::now() < deadline, "Diagnostic command timed out");
        let output_done = drain(&mut stdout, &mut output, stdout_limit)?;
        let errors_done = drain(&mut stderr, &mut errors, MAX_OUTPUT)?;
        if status.is_none() && child.exited()? {
            status = Some(child.finish()?);
        }
        if let Some(status) = status {
            if output_done && errors_done {
                return Ok(Output {
                    status,
                    stdout: String::from_utf8(output)
                        .context("Invalid diagnostic output encoding")?,
                    stderr: String::from_utf8_lossy(&errors).into_owned(),
                });
            }
        }
        let mut descriptors = [
            libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if output_done {
            descriptors[0].fd = -1;
        }
        if errors_done {
            descriptors[1].fd = -1;
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(25) as i32;
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, wait) };
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error()).context("Waiting for diagnostic output");
        }
    }
}

fn nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "Cannot make diagnostic output nonblocking"
    );
    Ok(())
}

fn drain(reader: &mut impl Read, output: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0; 4096];
    for _ in 0..16 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                ensure!(
                    output.len() + count <= limit,
                    "Helper output exceeded {} KiB",
                    limit / 1024
                );
                output.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => bail!("Reading diagnostic output: {error}"),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn captures_both_streams_and_preserves_unsuccessful_exit() {
        let result = run(
            shell("printf 'disabled\\n'; printf 'detail\\n' >&2; exit 1"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(result.stdout, "disabled\n");
        assert_eq!(result.stderr, "detail\n");
    }

    #[test]
    fn passes_file_input_without_a_pipe_writer_or_disk_path_argument() {
        use std::io::{Seek, Write};
        let mut input = tempfile::tempfile().unwrap();
        input
            .write_all(b"{\"path\":\"space and $shell text\"}")
            .unwrap();
        input.rewind().unwrap();
        let output = run_with_stdin(
            Command::new("/bin/cat"),
            input.into(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, "{\"path\":\"space and $shell text\"}");
    }

    #[test]
    fn output_flood_and_idle_child_are_bounded() {
        let began = Instant::now();
        assert!(run(
            shell("while :; do printf '1234567890'; done"),
            Duration::from_secs(2)
        )
        .unwrap_err()
        .to_string()
        .contains("64 KiB"));
        assert!(run(shell("sleep 10"), Duration::from_millis(100))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(began.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn expanded_stdout_preserves_the_default_and_stderr_limits() {
        use std::io::{Seek, Write};
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(&vec![b'x'; 70 * 1024]).unwrap();
        input.rewind().unwrap();
        let output = run_with_stdin_limit(
            Command::new("/bin/cat"),
            input.try_clone().unwrap().into(),
            Duration::from_secs(2),
            128 * 1024,
        )
        .unwrap();
        assert_eq!(output.stdout.len(), 70 * 1024);
        input.rewind().unwrap();
        assert!(run_with_stdin(
            Command::new("/bin/cat"),
            input.try_clone().unwrap().into(),
            Duration::from_secs(2)
        )
        .is_err());
        input.rewind().unwrap();
        assert!(run_with_stdin_limit(
            shell("cat >&2"),
            input.into(),
            Duration::from_secs(2),
            128 * 1024
        )
        .is_err());
        assert!(run_with_stdin_limit(
            Command::new("/missing-command"),
            Stdio::null(),
            Duration::from_secs(2),
            16 * 1024 * 1024 + 1
        )
        .unwrap_err()
        .to_string()
        .contains("limit"));
    }
}
