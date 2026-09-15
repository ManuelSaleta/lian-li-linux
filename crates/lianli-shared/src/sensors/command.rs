use super::SensorReading;
use anyhow::{ensure, Context, Result};
use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 8192;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const INTEREST_TTL: Duration = Duration::from_secs(15);

pub struct CommandSampler {
    state: Arc<SamplerState>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct SamplerState {
    entries: Mutex<HashMap<String, CommandEntry>>,
    wake: Condvar,
    stop: AtomicBool,
}

struct CommandEntry {
    requested_at: Instant,
    attempted_at: Option<Instant>,
    reading: Option<SensorReading>,
    error: Option<String>,
}

impl Default for CommandSampler {
    fn default() -> Self {
        Self {
            state: Arc::new(SamplerState::default()),
            worker: None,
        }
    }
}

impl CommandSampler {
    /// Registers a command and returns its latest sample without waiting for execution.
    /// Callers must apply their freshness policy to the retained observation time.
    pub fn reading(&mut self, command: &str) -> Result<SensorReading> {
        ensure!(command.len() <= 16 * 1024, "Sensor command exceeds 16 KiB");
        if self.worker.is_none() {
            let state = Arc::clone(&self.state);
            self.worker = Some(
                std::thread::Builder::new()
                    .name("cooling-sensors".into())
                    .spawn(move || sample_commands(state))
                    .context("Starting sensor sampler")?,
            );
        }
        let now = Instant::now();
        let mut entries = self
            .state
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, entry| now.saturating_duration_since(entry.requested_at) < INTEREST_TTL);
        let new_entry = !entries.contains_key(command);
        ensure!(
            !new_entry || entries.len() < 256,
            "Too many active sensor commands"
        );
        let entry = entries.entry(command.into()).or_insert(CommandEntry {
            requested_at: now,
            attempted_at: None,
            reading: None,
            error: None,
        });
        entry.requested_at = now;
        let result = entry.reading.ok_or_else(|| {
            anyhow::anyhow!(entry
                .error
                .clone()
                .unwrap_or_else(|| "Waiting for sensor command".into()))
        });
        drop(entries);
        if new_entry {
            self.state.wake.notify_one();
        }
        result
    }
}

impl Drop for CommandSampler {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        let guard = self
            .state
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.state.wake.notify_one();
        drop(guard);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("Sensor sampling worker panicked");
            }
        }
    }
}

fn sample_commands(state: Arc<SamplerState>) {
    loop {
        let mut entries = state
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let command = loop {
            if state.stop.load(Ordering::Relaxed) {
                return;
            }
            let now = Instant::now();
            entries.retain(|_, entry| {
                now.saturating_duration_since(entry.requested_at) < INTEREST_TTL
            });
            let next = entries
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .attempted_at
                        .is_none_or(|at| now.saturating_duration_since(at) >= COMMAND_TIMEOUT)
                })
                .min_by_key(|(_, entry)| entry.attempted_at)
                .map(|(command, _)| command.clone());
            if let Some(command) = next {
                entries.get_mut(&command).unwrap().attempted_at = Some(now);
                break command;
            }
            let wait = entries
                .values()
                .filter_map(|entry| entry.attempted_at)
                .map(|at| (at + COMMAND_TIMEOUT).saturating_duration_since(now))
                .min();
            entries = match wait {
                Some(wait) => {
                    state
                        .wake
                        .wait_timeout(entries, wait)
                        .unwrap_or_else(|error| error.into_inner())
                        .0
                }
                None => state
                    .wake
                    .wait(entries)
                    .unwrap_or_else(|error| error.into_inner()),
            };
        };
        drop(entries);
        let result = reading(&command, &state.stop);
        let mut entries = state
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = entries.get_mut(&command) {
            match result {
                Ok(reading) => {
                    entry.reading = Some(reading);
                    entry.error = None;
                }
                Err(error) => entry.error = Some(error.to_string()),
            }
        }
    }
}

pub(super) fn reading(script: &str, stop: &AtomicBool) -> Result<SensorReading> {
    let observed_at = Instant::now();
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    let output = output_until(command, COMMAND_TIMEOUT, stop)?;
    let text = std::str::from_utf8(&output).context("Sensor output is not UTF-8")?;
    let value: f32 = text
        .split_whitespace()
        .next()
        .context("Sensor output is empty")?
        .parse()?;
    ensure!(value.is_finite(), "Sensor value is not finite");
    Ok(SensorReading { value, observed_at })
}

pub(super) fn output(command: Command) -> Result<Vec<u8>> {
    output_until(command, COMMAND_TIMEOUT, &AtomicBool::new(false))
}

fn output_until(mut command: Command, timeout: Duration, stop: &AtomicBool) -> Result<Vec<u8>> {
    ensure!(!stop.load(Ordering::Relaxed), "Sensor command cancelled");
    let parent = std::process::id() as libc::pid_t;
    // This thread owns the child until it is reaped, including background NVIDIA queries.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            Ok(())
        });
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("Starting sensor command")?;
    let mut child = CommandProcess(Some(child));
    let mut pipe = child.0.as_mut().unwrap().stdout.take().unwrap();
    let fd = pipe.as_raw_fd();
    // The pipe is owned here and remains open until capture finishes.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0,
        "Reading sensor pipe flags: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "Setting sensor pipe nonblocking: {}",
        std::io::Error::last_os_error()
    );
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        ensure!(!stop.load(Ordering::Relaxed), "Sensor command cancelled");
        ensure!(Instant::now() < deadline, "Sensor command timed out");
        let mut buffer = [0; 1024];
        if !eof {
            match pipe.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(length) => {
                    ensure!(
                        output.len() + length <= OUTPUT_LIMIT,
                        "Sensor command output exceeds 8 KiB"
                    );
                    output.extend_from_slice(&buffer[..length]);
                    continue;
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(error) => return Err(error).context("Reading sensor command output"),
            }
        }
        if eof && child.exited()? {
            let status = child.finish()?;
            ensure!(status.success(), "Sensor command failed: {status}");
            return Ok(output);
        }
        let mut poll = libc::pollfd {
            fd: if eof { -1 } else { fd },
            events: libc::POLLIN,
            revents: 0,
        };
        let wait =
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now()));
        let result = unsafe { libc::poll(&mut poll, 1, wait.as_millis().max(1) as i32) };
        if result < 0 && std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error()).context("Waiting for sensor command");
        }
    }
}

struct CommandProcess(Option<Child>);

impl CommandProcess {
    fn exited(&self) -> Result<bool> {
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.0.as_ref().unwrap().id(),
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        ensure!(
            result == 0,
            "Inspecting sensor command: {}",
            std::io::Error::last_os_error()
        );
        Ok(unsafe { status.si_pid() } != 0)
    }

    fn finish(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut child = self.0.take().unwrap();
        // Keep the leader unreaped until group cleanup prevents PID reuse.
        if unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) } < 0 {
            let _ = child.kill();
        }
        child.wait()
    }
}

impl Drop for CommandProcess {
    fn drop(&mut self) {
        if self.0.is_some() {
            let _ = self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn command_sampler_reads_without_waiting_for_processes_and_stops_cleanly() {
        let mut sampler = CommandSampler::default();
        let started = Instant::now();
        assert!(sampler.reading("sleep 30").is_err());
        assert!(started.elapsed() < Duration::from_millis(250));
        let deadline = Instant::now() + Duration::from_secs(4);
        let reading = loop {
            let _ = sampler.reading("sleep 30");
            if let Ok(reading) = sampler.reading("printf 42") {
                break reading;
            }
            assert!(
                Instant::now() < deadline,
                "Slow command starved another sensor"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(reading.value, 42.0);
        assert!(reading.observed_at >= started);
        let stopped = Instant::now();
        drop(sampler);
        assert!(stopped.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn oversized_command_does_not_start_a_worker() {
        let mut sampler = CommandSampler::default();
        assert!(sampler.reading(&"x".repeat(16 * 1024 + 1)).is_err());
        assert!(sampler.worker.is_none());
    }

    #[test]
    fn sensor_commands_return_complete_output_and_reject_failures() {
        assert_eq!(output(shell("printf '42\\n'")).unwrap(), b"42\n");
        assert!(output(shell("exit 3")).is_err());
        assert!(output(shell("head -c 8193 /dev/zero"))
            .unwrap_err()
            .to_string()
            .contains("8 KiB"));
    }

    #[test]
    fn stuck_commands_and_inherited_pipes_have_bounded_lifetimes() {
        for script in ["sleep 30", "sleep 30 & printf 42"] {
            let started = Instant::now();
            let error = output_until(
                shell(script),
                Duration::from_millis(100),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(error.to_string().contains("timed out"));
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        assert!(output_until(
            shell("sleep 30"),
            Duration::from_secs(1),
            &AtomicBool::new(true)
        )
        .is_err());
    }
}
