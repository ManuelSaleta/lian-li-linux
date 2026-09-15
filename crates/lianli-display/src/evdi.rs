use crate::frame::{Frame, Mode, PixelFormat};
use crate::{Capture, Event, OutputRequest};
use anyhow::{ensure, Context, Result};
use lianli_evdi::{EvdiHandle, Event as EvdiEvent};
use std::time::Duration;

pub struct EvdiCapture {
    handle: EvdiHandle,
    request: OutputRequest,
    current: Option<(Mode, PixelFormat)>,
}

impl EvdiCapture {
    pub fn open(request: OutputRequest) -> Result<Self> {
        request.validate_mode(request.preferred)?;
        let mut handle = EvdiHandle::open_or_add().context("opening EVDI output")?;
        handle.set_buffer(
            request.preferred.width as i32,
            request.preferred.height as i32,
        )?;
        let area = request.max_width.saturating_mul(request.max_height).max(1);
        handle.connect_with_rate(&request.edid, area, 80_000_000)?;
        Ok(Self {
            handle,
            request,
            current: None,
        })
    }
}

impl Capture for EvdiCapture {
    fn invalidate(&mut self) -> Result<()> {
        Ok(())
    }

    fn poll_events(
        &mut self,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Event>> {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Relaxed),
            "EVDI capture cancelled"
        );
        let events = self.handle.poll_events(timeout)?;
        let mut output = Vec::with_capacity(events.len());
        for event in events {
            match event {
                EvdiEvent::ModeChanged(mode) => {
                    ensure!(
                        mode.width > 0 && mode.height > 0 && mode.refresh_hz > 0,
                        "Invalid EVDI mode"
                    );
                    ensure!(
                        mode.bits_per_pixel == 32,
                        "EVDI requires a 32-bit pixel format"
                    );
                    let format = PixelFormat::from_fourcc(mode.pixel_format)?;
                    let mode = Mode {
                        width: mode.width as u32,
                        height: mode.height as u32,
                        refresh_hz: mode.refresh_hz as u32,
                    };
                    self.request.validate_mode(mode)?;
                    self.handle
                        .set_buffer(mode.width as i32, mode.height as i32)?;
                    self.current = Some((mode, format));
                    output.push(Event::ModeChanged(mode, format));
                }
                EvdiEvent::UpdateReady(_) => output.push(Event::FrameReady),
                EvdiEvent::DpmsChanged(mode) => output.push(Event::PowerChanged(mode == 0)),
                EvdiEvent::CrtcStateChanged(_) => {}
            }
        }
        Ok(output)
    }

    fn request_update(&mut self) -> Result<bool> {
        Ok(self.handle.request_update())
    }

    fn frame(&mut self, _: &std::sync::atomic::AtomicBool) -> Result<Frame<'_>> {
        let (mode, format) = self.current.context("EVDI mode has not been negotiated")?;
        self.handle.grab_pixels();
        Frame::new(
            mode,
            format,
            mode.width as usize * 4,
            self.handle
                .pixels()
                .context("EVDI framebuffer unavailable")?,
        )
    }
}
