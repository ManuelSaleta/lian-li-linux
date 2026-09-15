mod enumerate;
mod policy;
mod session;
mod status;
mod worker;

pub use enumerate::{enumerate_turzx, TurzxDeviceMatch};

use lianli_devices::turzx;
use policy::SharedVideoPolicy;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use worker::spawn_worker;

/// Key identifying a single physical USB attachment (bus + address).
pub type DeviceKey = (u8, u8);

/// Handle to a running worker. Dropping it signals the worker to stop and
/// waits for it to join.
pub struct DesktopDisplayHandle {
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pid: u16,
}

impl Drop for DesktopDisplayHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            if let Err(e) = j.join() {
                warn!(
                    "TURZX {:04x}:{:04x} worker panicked on shutdown: {e:?}",
                    turzx::VID,
                    self.pid
                );
            }
        }
    }
}

const MAX_START_ATTEMPTS: u32 = 5;
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

struct DisplayWorker {
    pid: u16,
    retiring: bool,
    retry_requested: bool,
    status: Arc<status::StreamStatus>,
    handle: Option<DesktopDisplayHandle>,
    retry: RetryState,
}

struct RetryState {
    failures: u32,
    retry_at: Instant,
}

impl RetryState {
    fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            retry_at: now,
        }
    }

    fn ready(&self, now: Instant) -> bool {
        self.failures < MAX_START_ATTEMPTS && now >= self.retry_at
    }

    fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let delay = 5u64.saturating_mul(1u64 << self.failures.saturating_sub(1).min(4));
        self.retry_at = now + Duration::from_secs(delay.min(60));
    }
}

#[derive(Default)]
pub struct DesktopDisplayRegistry {
    switching: HashSet<DeviceKey>,
    workers: HashMap<DeviceKey, DisplayWorker>,
    video_policy: Arc<SharedVideoPolicy>,
    session: Option<session::SessionCoordinator>,
    session_epoch: u64,
}

impl DesktopDisplayRegistry {
    pub fn retry(&mut self, key: DeviceKey, pid: u16) -> bool {
        if self.switching.contains(&key) {
            return false;
        }
        let Some(worker) = self.workers.get_mut(&key) else {
            return false;
        };
        if worker.retiring
            || worker.pid != pid
            || worker.status.snapshot().state != lianli_shared::ipc::DesktopStreamState::Failed
        {
            return false;
        }
        worker.retry_requested = worker.handle.is_some();
        worker.retry = RetryState::new(Instant::now());
        worker.status.waiting();
        true
    }

    pub fn statuses(&self) -> Vec<lianli_shared::ipc::DesktopStreamStatus> {
        let mut states: Vec<_> = self
            .workers
            .values()
            .filter(|worker| !worker.retiring)
            .map(|worker| worker.status.snapshot())
            .collect();
        states.sort_by_key(|state| (state.bus, state.address));
        states
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_video_policy(&self, enabled: bool, fps_limit: f32) {
        self.video_policy.set(enabled, fps_limit);
    }

    pub fn start_session(&mut self, socket: &std::path::Path) -> anyhow::Result<()> {
        self.session = Some(session::SessionCoordinator::start(socket)?);
        Ok(())
    }

    pub fn sync(&mut self, present: &[TurzxDeviceMatch]) {
        let epoch = self
            .session
            .as_ref()
            .map_or(0, |session| session.client.epoch());
        if epoch != self.session_epoch {
            self.session_epoch = epoch;
            for worker in self.workers.values_mut() {
                worker.retry = RetryState::new(Instant::now());
            }
        }
        self.retire_obsolete(
            &present
                .iter()
                .map(|target| (target.key, target.pid))
                .collect(),
        );

        for target in present {
            if self.switching.contains(&target.key) {
                continue;
            }
            let now = Instant::now();
            let worker = self
                .workers
                .entry(target.key)
                .or_insert_with(|| DisplayWorker {
                    pid: target.pid,
                    retiring: false,
                    retry_requested: false,
                    status: Arc::new(status::StreamStatus::new(target.key, target.pid)),
                    handle: None,
                    retry: RetryState::new(now),
                });
            if worker.retiring {
                continue;
            }
            if let Some(handle) = &worker.handle {
                if handle.join.as_ref().is_some_and(|join| !join.is_finished()) {
                    continue;
                }
                let healthy = handle.healthy.load(Ordering::Relaxed);
                drop(worker.handle.take());
                if worker.retry_requested {
                    worker.retry_requested = false;
                    worker.retry = RetryState::new(now);
                } else {
                    if worker.status.snapshot().state
                        != lianli_shared::ipc::DesktopStreamState::Failed
                    {
                        worker
                            .status
                            .failed("Desktop worker exited without a completion result");
                    }
                    if healthy {
                        worker.retry = RetryState::new(now);
                    }
                    worker.retry.failed(now);
                }
                if worker.retry.failures >= MAX_START_ATTEMPTS {
                    warn!("TURZX {:?} stopped after {MAX_START_ATTEMPTS} failed starts; repair the backend and choose Retry desktop display in Installation Health", target.key);
                }
            }
            if !worker.retry.ready(now) {
                continue;
            }
            let Some(session) = self
                .session
                .as_ref()
                .filter(|session| session.client.ready())
            else {
                continue;
            };
            match spawn_worker(
                target.clone(),
                Arc::clone(&self.video_policy),
                session.client.clone(),
                Arc::clone(&worker.status),
            ) {
                Ok(handle) => {
                    info!(
                        "TURZX {:04x}:{:04x} at {:?} worker spawned",
                        turzx::VID,
                        target.pid,
                        target.key
                    );
                    worker.handle = Some(handle);
                }
                Err(error) => {
                    worker.status.failed(&format!("{error:#}"));
                    worker.retry.failed(now);
                    warn!(
                        "TURZX {:04x}:{:04x} at {:?} spawn failed: {error:#}",
                        turzx::VID,
                        target.pid,
                        target.key
                    );
                }
            }
        }
    }

    fn retire_obsolete(&mut self, present: &HashMap<DeviceKey, u16>) {
        self.workers.retain(|key, worker| {
            if !worker.retiring && present.get(key) == Some(&worker.pid) {
                return true;
            }
            worker.retiring = true;
            worker.handle.as_ref().is_some_and(|handle| {
                handle.stop.store(true, Ordering::SeqCst);
                handle.join.as_ref().is_some_and(|join| !join.is_finished())
            })
        });
    }

    pub fn stop_for_device(&mut self, key: DeviceKey) -> bool {
        self.switching.insert(key);
        if let Some(worker) = self.workers.get_mut(&key) {
            if let Some(handle) = &worker.handle {
                handle.stop.store(true, Ordering::SeqCst);
                if handle.join.as_ref().is_some_and(|join| !join.is_finished()) {
                    return false;
                }
            }
            drop(worker.handle.take());
            worker.status.waiting();
            worker.retry_requested = false;
            worker.retry = RetryState::new(Instant::now() + Duration::from_secs(8));
        }
        true
    }

    pub fn finish_switch(&mut self, key: DeviceKey) {
        self.switching.remove(&key);
        if let Some(worker) = self.workers.get_mut(&key) {
            worker.retry = RetryState::new(Instant::now() + Duration::from_secs(8));
        }
    }

    pub fn shutdown(&mut self) {
        for worker in self.workers.values() {
            if let Some(handle) = &worker.handle {
                handle.stop.store(true, Ordering::SeqCst);
            }
        }
        self.workers.clear();
        self.switching.clear();
        self.session = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_or_replaced_attachments_retire_without_blocking_or_reopening() {
        for replacement in [None, Some(0xad26)] {
            let (release, wait) = std::sync::mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let mut registry = DesktopDisplayRegistry::new();
            registry.workers.insert(
                (1, 2),
                DisplayWorker {
                    pid: 0xad21,
                    retiring: false,
                    retry_requested: false,
                    status: Arc::new(status::StreamStatus::new((1, 2), 0xad21)),
                    retry: RetryState::new(Instant::now()),
                    handle: Some(DesktopDisplayHandle {
                        stop: Arc::clone(&stop),
                        healthy: Arc::new(AtomicBool::new(false)),
                        join: Some(std::thread::spawn(move || {
                            let _ = wait.recv_timeout(Duration::from_secs(2));
                        })),
                        pid: 0xad21,
                    }),
                },
            );
            let present = replacement.map(|pid| ((1, 2), pid)).into_iter().collect();
            let started = Instant::now();
            registry.retire_obsolete(&present);
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(stop.load(Ordering::SeqCst));
            assert!(registry.workers[&(1, 2)].retiring);
            assert!(registry.statuses().is_empty());
            assert!(!registry.retry((1, 2), 0xad21));
            registry.retire_obsolete(&HashMap::from([((1, 2), 0xad21)]));
            assert!(registry.workers[&(1, 2)].retiring);
            release.send(()).unwrap();
            registry
                .workers
                .get_mut(&(1, 2))
                .unwrap()
                .handle
                .as_mut()
                .unwrap()
                .join
                .take()
                .unwrap()
                .join()
                .unwrap();
            registry.retire_obsolete(&present);
            assert!(registry.workers.is_empty());
        }
    }

    #[test]
    fn explicit_retry_is_scoped_and_does_not_join_a_cleaning_worker() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut registry = DesktopDisplayRegistry::new();
        let status = Arc::new(status::StreamStatus::new((1, 2), 0xad21));
        status.failed("Capture failed");
        registry.workers.insert(
            (1, 2),
            DisplayWorker {
                pid: 0xad21,
                retiring: false,
                retry_requested: false,
                status,
                retry: RetryState {
                    failures: MAX_START_ATTEMPTS,
                    retry_at: Instant::now(),
                },
                handle: Some(DesktopDisplayHandle {
                    stop: Arc::new(AtomicBool::new(false)),
                    healthy: Arc::new(AtomicBool::new(false)),
                    join: Some(std::thread::spawn(move || {
                        wait.recv().unwrap();
                    })),
                    pid: 0xad21,
                }),
            },
        );
        assert!(!registry.retry((1, 3), 0xad21));
        assert!(!registry.retry((1, 2), 0xad22));
        let started = Instant::now();
        let accepted = registry.retry((1, 2), 0xad21);
        release.send(()).unwrap();
        assert!(accepted);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(registry.workers[&(1, 2)].retry_requested);
        assert_eq!(registry.workers[&(1, 2)].retry.failures, 0);
        assert!(!registry.retry((1, 2), 0xad21));
        assert_eq!(
            registry.statuses()[0].state,
            lianli_shared::ipc::DesktopStreamState::WaitingForSession
        );
    }

    #[test]
    fn startup_retries_back_off_and_stop_at_the_limit() {
        let mut now = Instant::now();
        let mut retry = RetryState::new(now);
        assert!(retry.ready(now));
        for seconds in [5, 10, 20, 40] {
            retry.failed(now);
            assert!(!retry.ready(now + Duration::from_secs(seconds - 1)));
            now += Duration::from_secs(seconds);
            assert!(retry.ready(now));
        }
        retry.failed(now);
        assert!(!retry.ready(now + Duration::from_secs(3600)));
        assert!(RetryState::new(now).ready(now));
    }

    #[test]
    fn mode_switch_only_stops_the_selected_attachment() {
        let now = Instant::now();
        let mut registry = DesktopDisplayRegistry::new();
        let mut stops = Vec::new();
        for key in [(1, 2), (1, 3)] {
            let stop = Arc::new(AtomicBool::new(false));
            stops.push(Arc::clone(&stop));
            registry.workers.insert(
                key,
                DisplayWorker {
                    pid: 0xad21,
                    retiring: false,
                    retry_requested: false,
                    status: Arc::new(status::StreamStatus::new(key, 0xad21)),
                    handle: Some(DesktopDisplayHandle {
                        stop,
                        healthy: Arc::new(AtomicBool::new(false)),
                        join: None,
                        pid: 0xad21,
                    }),
                    retry: RetryState::new(now),
                },
            );
        }
        assert!(registry.stop_for_device((1, 2)));
        assert!(stops[0].load(Ordering::SeqCst));
        assert!(!stops[1].load(Ordering::SeqCst));
        assert!(!registry.workers[&(1, 2)].retry.ready(Instant::now()));
        registry.shutdown();
        assert!(stops[1].load(Ordering::SeqCst));
    }

    #[test]
    fn mode_switch_reserves_attachment_without_waiting_for_capture_teardown() {
        let mut registry = DesktopDisplayRegistry::new();
        let (release, wait) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        registry.workers.insert(
            (1, 2),
            DisplayWorker {
                pid: 0xad21,
                retiring: false,
                retry_requested: false,
                status: Arc::new(status::StreamStatus::new((1, 2), 0xad21)),
                retry: RetryState::new(Instant::now()),
                handle: Some(DesktopDisplayHandle {
                    stop: Arc::clone(&stop),
                    healthy: Arc::new(AtomicBool::new(false)),
                    join: Some(thread_fixture(wait)),
                    pid: 0xad21,
                }),
            },
        );
        let started = Instant::now();
        assert!(!registry.stop_for_device((1, 2)));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(stop.load(Ordering::SeqCst));
        assert!(registry.switching.contains(&(1, 2)));
        assert!(!registry.retry((1, 2), 0xad21));
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !registry.stop_for_device((1, 2)) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(registry.workers[&(1, 2)].handle.is_none());
        assert!(registry.switching.contains(&(1, 2)));
        registry.finish_switch((1, 2));
        assert!(!registry.switching.contains(&(1, 2)));
        assert!(!registry.workers[&(1, 2)].retry.ready(Instant::now()));
    }

    fn thread_fixture(wait: std::sync::mpsc::Receiver<()>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        })
    }
}
