use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use lianli_shared::display::DisplayVideoPolicy as VideoPolicy;

pub(super) struct SharedVideoPolicy(AtomicU32);

impl Default for SharedVideoPolicy {
    fn default() -> Self {
        Self(AtomicU32::new(30))
    }
}

impl SharedVideoPolicy {
    pub fn set(&self, hardware_video: bool, fps_limit: f32) {
        let fps = if fps_limit.is_finite() {
            fps_limit.clamp(1.0, 120.0) as u32
        } else {
            30
        };
        self.0
            .store(fps | (u32::from(hardware_video) << 8), Ordering::Release);
    }

    pub fn load(&self) -> VideoPolicy {
        let value = self.0.load(Ordering::Acquire);
        VideoPolicy {
            hardware_video: value & 256 != 0,
            fps_limit: value & 255,
        }
    }
}

pub(super) struct FramePacer {
    interval: Duration,
    last_frame: Option<Instant>,
}

impl FramePacer {
    pub fn new() -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / 30.0),
            last_frame: None,
        }
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.interval = Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 127)));
    }

    pub fn ready(&self, now: Instant) -> bool {
        self.last_frame
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval)
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.last_frame
            .map(|last| (last + self.interval).saturating_duration_since(now))
            .unwrap_or_default()
    }

    pub fn started(&mut self, now: Instant) {
        self.last_frame = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_policy_defaults_and_clamps_to_both_user_and_source_limits() {
        let policy = SharedVideoPolicy::default();
        assert!(!policy.load().hardware_video);
        assert_eq!(policy.load().fps(60), 30);
        assert_eq!(policy.load().fps(15), 15);
        policy.set(true, 9.9);
        assert!(policy.load().hardware_video);
        assert_eq!(policy.load().fps(60), 9);
        policy.set(false, f32::NAN);
        assert_eq!(policy.load().fps(60), 30);
        policy.set(false, 1000.0);
        assert_eq!(policy.load().fps(127), 120);
    }

    #[test]
    fn fps_changes_preserve_pacing_and_never_accumulate_catch_up_frames() {
        let now = Instant::now();
        let mut pacer = FramePacer::new();
        pacer.set_fps(10);
        assert!(pacer.ready(now));
        pacer.started(now);
        assert!(!pacer.ready(now + Duration::from_millis(99)));
        assert!(pacer.ready(now + Duration::from_millis(100)));
        pacer.set_fps(1);
        assert!(!pacer.ready(now + Duration::from_millis(100)));
        assert_eq!(
            pacer.remaining(now + Duration::from_millis(100)),
            Duration::from_millis(900)
        );
        let delayed = now + Duration::from_secs(30);
        assert!(pacer.ready(delayed));
        pacer.started(delayed);
        assert!(!pacer.ready(delayed));
    }
}
