use crate::video::process::output_cancellable;
use crate::MediaError;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct PreparationControl {
    pub hardware_video: bool,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl PreparationControl {
    pub fn new(hardware_video: bool) -> Self {
        Self {
            hardware_video,
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs(180),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn next_asset(&self) -> Self {
        Self {
            hardware_video: self.hardware_video,
            cancelled: Arc::clone(&self.cancelled),
            deadline: Instant::now() + Duration::from_secs(180),
        }
    }

    pub fn check(&self) -> Result<(), MediaError> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(MediaError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(MediaError::HelperTimedOut(
                "Media preparation exceeded three minutes".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn output(&self, command: Command, timeout: Duration) -> Result<Output, MediaError> {
        self.check()?;
        Ok(output_cancellable(
            command,
            timeout.min(self.deadline.saturating_duration_since(Instant::now())),
            &self.cancelled,
        )?)
    }

    pub(crate) fn stream_frames(
        &self,
        command: Command,
        frame_bytes: usize,
        consume: impl FnMut(&[u8]) -> Result<(), MediaError>,
    ) -> Result<Output, MediaError> {
        self.check()?;
        crate::video::process::stream_frames(
            command,
            crate::video::process::ENCODE_TIMEOUT
                .min(self.deadline.saturating_duration_since(Instant::now())),
            &self.cancelled,
            frame_bytes,
            consume,
        )
    }
}

impl From<bool> for PreparationControl {
    fn from(hardware_video: bool) -> Self {
        Self::new(hardware_video)
    }
}

impl From<&PreparationControl> for PreparationControl {
    fn from(control: &PreparationControl) -> Self {
        control.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_job_stops_nested_preparation_before_it_opens_files() {
        let control = PreparationControl::new(false);
        let nested = control.clone();
        control.cancel();
        let config = serde_json::from_value(serde_json::json!({
            "type": "video", "path": "/nonexistent/should-not-be-opened.mp4"
        }))
        .unwrap();
        let result = crate::prepare_media_asset(
            &config,
            30.0,
            &lianli_shared::screen::ScreenInfo::WIRELESS_LCD,
            true,
            &[],
            &[],
            nested,
        );
        assert!(matches!(result, Err(MediaError::Cancelled)));
    }
}
