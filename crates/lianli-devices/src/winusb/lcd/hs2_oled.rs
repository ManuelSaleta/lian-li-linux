use anyhow::Result;
use lianli_shared::screen::ScreenInfo;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration as StdDuration;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::core::{SharedTransport, WinUsbLcdCore};
use super::trait_::{BoxedWinUsbLcd, WinUsbLcd};

const WRITE_TIMEOUT: Duration = Duration::from_millis(2_000);
const READ_TIMEOUT: Duration = Duration::from_millis(200);
const STOP_CLOCK_ACK_POLLS: u32 = 40;

pub struct Hs2OledWinUsbLcd {
    core: WinUsbLcdCore,
    play_tick: u32,
}

impl Hs2OledWinUsbLcd {
    pub(crate) fn new(
        device: rusb::Device<rusb::GlobalContext>,
        screen: ScreenInfo,
        name: &str,
    ) -> Result<Self> {
        let core = WinUsbLcdCore::open(device, screen, name, WRITE_TIMEOUT, READ_TIMEOUT)?;
        Ok(Self { core, play_tick: 0 })
    }

    pub(crate) fn from_shared(
        transport: SharedTransport,
        screen: ScreenInfo,
        name: String,
    ) -> Self {
        Self {
            core: WinUsbLcdCore::from_shared(transport, screen, name, WRITE_TIMEOUT, READ_TIMEOUT),
            play_tick: 0,
        }
    }

    fn do_init(&mut self) -> Result<()> {
        self.core.init_logging();
        self.core.reset_failure_state();
        self.core.read_firmware();

        let warn = self.core.builder_mut().warn_switch_header_winusb(false);
        self.core.send_command(warn, "WarnSwitch");

        let stop_play = self.core.builder_mut().stop_play_header_winusb();
        self.core.send_command(stop_play, "StopPlay");

        let hide_show = self
            .core
            .builder_mut()
            .hide_show_frames_none_header_winusb();
        self.core.send_command(hide_show, "SetHideShowFramesNone");

        let sync = self.core.builder_mut().sync_clock_header_winusb(2);
        self.core.send_command(sync, "SyncClock");

        let mut _drained = false;
        for _ in 0..STOP_CLOCK_ACK_POLLS {
            match self.core.stop_clock_resp() {
                Some(resp) if resp.len() > 9 && resp[8] == 0xFF && resp[9] == 0xFF => {
                    _drained = true;
                    break;
                }
                Some(_) => std::thread::sleep(StdDuration::from_millis(50)),
                None => break,
            }
        }

        self.core.clear_png_cmd();
        self.core.clear_png_cmd();
        self.core.clear_jpg_layer();

        self.core.query_h264_block();
        self.core.initialized = true;
        Ok(())
    }

    fn capture_play_tick(&mut self) {
        const DOTNET_EPOCH_OFFSET_100NS: u64 = 621_355_680_000_000_000;
        let ticks100ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() / 100)
            .unwrap_or(0) as u64
            + DOTNET_EPOCH_OFFSET_100NS;
        self.play_tick = (ticks100ns & 0xFFFF_FFFF) as u32;
    }
}

impl WinUsbLcd for Hs2OledWinUsbLcd {
    fn screen_info(&self) -> &ScreenInfo {
        self.core.screen()
    }
    fn firmware_str(&self) -> Option<&str> {
        self.core.firmware_str()
    }
    fn shared_transport(&self) -> SharedTransport {
        self.core.shared_transport()
    }
    fn transport_release(&self) {
        self.core.transport_release()
    }
    fn initialize(&mut self) -> Result<()> {
        if !self.core.initialized {
            self.do_init()?;
        }
        Ok(())
    }
    fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        if !self.core.initialized {
            self.do_init()?;
        }
        self.core.send_frame(frame)
    }
    fn send_frame_verified(&mut self, frame: &[u8]) -> Result<()> {
        if !self.core.initialized {
            self.do_init()?;
        }
        self.core.send_frame_verified(frame)
    }
    fn set_brightness_val(&mut self, brightness: u8) -> Result<()> {
        self.core.set_brightness_val(brightness)
    }
    fn switch_to_desktop_mode(&mut self) -> Result<()> {
        self.core.switch_to_desktop_mode()
    }
    fn stream_h264(
        &mut self,
        path: &Path,
        looping: bool,
        stop: &AtomicBool,
        fps: f32,
    ) -> Result<()> {
        if !self.core.initialized {
            self.do_init()?;
        }
        self.capture_play_tick();
        self.core.apply_stream_fps(fps)?;
        self.core.stream_h264(
            path,
            looping,
            stop,
            fps,
            self.core.screen().play_count,
            self.play_tick,
        )
    }
    fn stream_h264_reader(
        &mut self,
        reader: &mut dyn std::io::Read,
        stop: &AtomicBool,
        fps: f32,
    ) -> Result<()> {
        if !self.core.initialized {
            self.do_init()?;
        }
        self.capture_play_tick();
        self.core.apply_stream_fps(fps)?;
        self.core
            .stream_h264_reader(reader, stop, self.core.screen().play_count, self.play_tick)
    }
}

pub(crate) fn boxed(
    device: rusb::Device<rusb::GlobalContext>,
    screen: ScreenInfo,
    name: &str,
) -> Result<BoxedWinUsbLcd> {
    Ok(Box::new(Hs2OledWinUsbLcd::new(device, screen, name)?))
}
