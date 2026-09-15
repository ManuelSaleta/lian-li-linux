use super::DaemonEvent;
use parking_lot::{Condvar, Mutex};
use signal_hook::iterator::{Handle, Signals};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

pub struct SignalMonitor {
    handle: Handle,
    requested: Arc<AtomicBool>,
    sender: Arc<Mutex<Option<Sender<DaemonEvent>>>>,
    worker: Option<JoinHandle<()>>,
}

impl SignalMonitor {
    pub fn new() -> std::io::Result<Self> {
        let mut signals =
            Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])?;
        let handle = signals.handle();
        let requested = Arc::new(AtomicBool::new(false));
        let sender = Arc::new(Mutex::new(None::<Sender<DaemonEvent>>));
        let pending = requested.clone();
        let destination = sender.clone();
        let worker = thread::Builder::new()
            .name("daemon-signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if pending.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    lianli_transport::usb::SHUTTING_DOWN.store(true, Ordering::Relaxed);
                    if let Some(tx) = destination.lock().as_ref() {
                        let _ = tx.send(DaemonEvent::Shutdown);
                    }
                    info!("Received signal {signal}; waiting for ordered daemon shutdown");
                }
            })?;
        Ok(Self {
            handle,
            requested,
            sender,
            worker: Some(worker),
        })
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(super) fn attach(&self, sender: Sender<DaemonEvent>) {
        let mut destination = self.sender.lock();
        if self.requested() {
            let _ = sender.send(DaemonEvent::Shutdown);
        }
        *destination = Some(sender);
    }
}

impl Drop for SignalMonitor {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                warn!("Daemon signal monitor panicked");
            }
        }
    }
}

#[derive(Default)]
struct State {
    operation: Option<(&'static str, Instant)>,
    warnings: usize,
    closed: bool,
}

impl State {
    fn warning(&mut self, now: Instant) -> Option<(&'static str, Duration)> {
        let (label, started) = self.operation?;
        let elapsed = now.saturating_duration_since(started);
        let reached = [5, 30, 120]
            .into_iter()
            .filter(|seconds| elapsed >= Duration::from_secs(*seconds))
            .count();
        if reached <= self.warnings {
            return None;
        }
        self.warnings = reached;
        Some((label, elapsed))
    }
}

pub(super) struct OperationMonitor {
    state: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl OperationMonitor {
    pub fn new() -> std::io::Result<Self> {
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let pending = state.clone();
        let worker = thread::Builder::new()
            .name("daemon-watchdog".into())
            .spawn(move || loop {
                let warning = {
                    let (lock, wake) = &*pending;
                    let mut state = lock.lock();
                    if !state.closed {
                        wake.wait_for(&mut state, Duration::from_secs(1));
                    }
                    if state.closed {
                        break;
                    }
                    state.warning(Instant::now())
                };
                if let Some((label, elapsed)) = warning {
                    if elapsed >= Duration::from_secs(120) {
                        error!("Daemon operation '{label}' has taken {}s; waiting for completion without forcing exit", elapsed.as_secs());
                    } else {
                        warn!("Daemon operation '{label}' has taken {}s", elapsed.as_secs());
                    }
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }

    pub fn enter(&mut self, label: &'static str) -> OperationGuard<'_> {
        let mut state = self.state.0.lock();
        state.operation = Some((label, Instant::now()));
        state.warnings = 0;
        drop(state);
        OperationGuard(self)
    }
}

pub(super) struct OperationGuard<'a>(&'a mut OperationMonitor);

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.0.state.0.lock().operation = None;
    }
}

impl Drop for OperationMonitor {
    fn drop(&mut self) {
        self.state.0.lock().closed = true;
        self.state.1.notify_one();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                warn!("Daemon operation monitor panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_is_silent_and_slow_shutdown_only_emits_bounded_warnings() {
        let started = Instant::now();
        let mut state = State::default();
        assert!(state.warning(started + Duration::from_secs(1000)).is_none());
        state.operation = Some(("shutdown", started));
        for seconds in [5, 30, 120] {
            assert_eq!(
                state
                    .warning(started + Duration::from_secs(seconds))
                    .unwrap()
                    .0,
                "shutdown"
            );
            assert!(state
                .warning(started + Duration::from_secs(seconds))
                .is_none());
        }
        assert!(state.warning(started + Duration::from_secs(3600)).is_none());
        assert!(!state.closed);
        assert_eq!(state.operation, Some(("shutdown", started)));
    }

    #[test]
    fn finishing_operations_clears_progress_and_dropping_monitor_joins_its_worker() {
        let mut monitor = OperationMonitor::new().unwrap();
        let state = monitor.state.clone();
        {
            let _operation = monitor.enter("startup");
            assert!(state.0.lock().operation.is_some());
        }
        assert!(state.0.lock().operation.is_none());
        drop(monitor);
        assert!(state.0.lock().closed);
        assert_eq!(Arc::strong_count(&state), 1);
    }
}
