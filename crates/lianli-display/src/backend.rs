use crate::{evdi::EvdiCapture, Capture, OutputRequest};
use anyhow::{ensure, Result};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct LocalBackend;

pub struct OpenedCapture {
    pub capture: Box<dyn Capture>,
    pub backend: &'static str,
    pub fallback_reason: Option<String>,
}

impl LocalBackend {
    pub fn discover() -> Result<Self> {
        Ok(Self)
    }

    pub fn open(self, request: OutputRequest, cancel: &AtomicBool) -> Result<OpenedCapture> {
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Desktop capture startup cancelled"
        );
        Ok(OpenedCapture {
            capture: Box::new(EvdiCapture::open(request)?),
            backend: "EVDI",
            fallback_reason: None,
        })
    }
}
