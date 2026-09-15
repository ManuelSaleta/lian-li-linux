use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Watchdog {
    epoch: Instant,
    last_progress: AtomicU64,
}

impl Watchdog {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_progress: AtomicU64::new(0),
        }
    }

    pub fn progress(&self, now: Instant) {
        self.last_progress
            .store(self.millis(now), Ordering::Relaxed);
    }

    pub fn stalled(&self, now: Instant) -> bool {
        self.millis(now)
            .saturating_sub(self.last_progress.load(Ordering::Relaxed))
            >= 20_000
    }

    fn millis(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch)
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}

pub fn restart_capture_process(reason: &str) -> ! {
    tracing::error!("Restarting the supervised capture process: {reason}");
    // Foreign GPU calls cannot be interrupted safely by joining a thread; this process owns no USB.
    unsafe { libc::_exit(1) }
}

pub const STOP_GRACE: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_progress_extends_the_deadline_but_a_stuck_operation_does_not() {
        let watch = Watchdog::new();
        let first = watch.epoch;
        assert!(!watch.stalled(first + Duration::from_secs(19)));
        assert!(watch.stalled(first + Duration::from_secs(20)));
        watch.progress(first + Duration::from_secs(19));
        assert!(!watch.stalled(first + Duration::from_secs(38)));
        assert!(watch.stalled(first + Duration::from_secs(39)));
    }
}
