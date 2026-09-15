//! Auto-attaches evdi virtual displays to connected Lian Li TURZX panels.
//!
//! For each `(VID=0x1A86, PID ∈ 0xAD10..0xAD3F)` device on the bus we run a
//! dedicated worker thread. The worker opens the USB panel via
//! [`TurzxDisplay`], spins up an evdi display node fed with the device's own
//! EDID, encodes framebuffer updates to H.264 via libavcodec, and pushes the
//! packets as TURZX stream A.

mod enumerate;
mod worker;

pub use enumerate::{enumerate_turzx, TurzxDeviceMatch};

use lianli_devices::turzx;
use lianli_media::video::ensure_ffmpeg_initialized;
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
    workers: HashMap<DeviceKey, DisplayWorker>,
    hardware_video: Arc<AtomicBool>,
}

impl DesktopDisplayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_hardware_video(&self, enabled: bool) {
        self.hardware_video.store(enabled, Ordering::Release);
    }

    pub fn sync(&mut self, present: &[TurzxDeviceMatch]) {
        let present_keys: HashSet<DeviceKey> = present.iter().map(|m| m.key).collect();
        for (key, worker) in &self.workers {
            if !present_keys.contains(key) {
                if let Some(handle) = &worker.handle {
                    handle.stop.store(true, Ordering::SeqCst);
                }
            }
        }
        self.workers.retain(|key, _| present_keys.contains(key));

        for target in present {
            let now = Instant::now();
            let worker = self
                .workers
                .entry(target.key)
                .or_insert_with(|| DisplayWorker {
                    pid: target.pid,
                    handle: None,
                    retry: RetryState::new(now),
                });
            if worker.pid != target.pid {
                worker.handle = None;
                worker.pid = target.pid;
                worker.retry = RetryState::new(now);
            }
            if let Some(handle) = &worker.handle {
                if handle.join.as_ref().is_some_and(|join| !join.is_finished()) {
                    continue;
                }
                let healthy = handle.healthy.load(Ordering::Relaxed);
                drop(worker.handle.take());
                if healthy {
                    worker.retry = RetryState::new(now);
                }
                worker.retry.failed(now);
                if worker.retry.failures >= MAX_START_ATTEMPTS {
                    warn!("TURZX {:?} stopped after {MAX_START_ATTEMPTS} failed starts; repair the backend and reconnect the panel or restart the daemon", target.key);
                }
            }
            if !worker.retry.ready(now) {
                continue;
            }
            match spawn_worker(target.clone(), Arc::clone(&self.hardware_video)) {
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

    pub fn stop_for_device(&mut self, key: DeviceKey) {
        if let Some(worker) = self.workers.get_mut(&key) {
            drop(worker.handle.take());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        registry.stop_for_device((1, 2));
        assert!(stops[0].load(Ordering::SeqCst));
        assert!(!stops[1].load(Ordering::SeqCst));
        assert!(!registry.workers[&(1, 2)].retry.ready(Instant::now()));
        registry.shutdown();
        assert!(stops[1].load(Ordering::SeqCst));
    }
}
