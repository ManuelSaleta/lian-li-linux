use anyhow::Result;
use lianli_shared::screen::ScreenInfo;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use super::core::SharedTransport;

pub(crate) trait WinUsbLcd: Send + Sync {
    fn screen_info(&self) -> &ScreenInfo;
    fn firmware_str(&self) -> Option<&str>;
    fn shared_transport(&self) -> SharedTransport;
    fn transport_release(&self);

    fn initialize(&mut self) -> Result<()>;
    fn send_frame(&mut self, frame: &[u8]) -> Result<()>;
    fn send_frame_verified(&mut self, frame: &[u8]) -> Result<()>;
    fn set_brightness_val(&mut self, brightness: u8) -> Result<()>;
    fn switch_to_desktop_mode(&mut self) -> Result<()>;

    fn stream_h264(
        &mut self,
        path: &Path,
        looping: bool,
        stop: &AtomicBool,
        fps: f32,
    ) -> Result<()>;
    fn stream_h264_reader(
        &mut self,
        reader: &mut dyn std::io::Read,
        stop: &AtomicBool,
        fps: f32,
    ) -> Result<()>;
}

pub(crate) type BoxedWinUsbLcd = Box<dyn WinUsbLcd + Send + Sync>;
