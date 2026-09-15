pub mod backend;
pub mod buffer;
pub mod channel;
pub mod evdi;
pub mod frame;
pub mod hyprland;
pub mod login;
mod socket;
pub mod wayland;

use anyhow::Result;
use frame::{Frame, Mode, PixelFormat};
use std::time::Duration;

pub use lianli_shared::display::{OutputRequest, MAX_FRAME_BYTES};

#[derive(Debug, Clone, Copy)]
pub enum Event {
    ModeChanged(Mode, PixelFormat),
    FrameReady,
    PowerChanged(bool),
}

pub trait Capture {
    fn poll_events(
        &mut self,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Event>>;
    fn request_update(&mut self) -> Result<bool>;
    fn frame(&mut self, cancel: &std::sync::atomic::AtomicBool) -> Result<Frame<'_>>;
    fn invalidate(&mut self) -> Result<()>;
}
