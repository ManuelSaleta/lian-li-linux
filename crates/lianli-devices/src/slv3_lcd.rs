use crate::crypto::PacketBuilder;
use anyhow::{bail, Context, Result};
use lianli_shared::screen::ScreenInfo;
use lianli_transport::usb::{RusbBulk, LCD_WRITE_TIMEOUT, USB_TIMEOUT};
use rusb::{Device, GlobalContext};
use tracing::{debug, info};

/// SLV3/TLV2 wireless LCD fan — USB bulk with DES-encrypted headers.
pub struct Slv3LcdDevice {
    transport: RusbBulk,
    bus: u8,
    address: u8,
    serial: String,
    initialized: bool,
    screen: ScreenInfo,
    firmware: Option<String>,
    new_lcd: Option<bool>,
}

impl Slv3LcdDevice {
    pub fn new(device: Device<GlobalContext>) -> Result<Self> {
        let bus = device.bus_number();
        let address = device.address();

        let desc = device
            .device_descriptor()
            .context("reading device descriptor")?;
        let serial = device
            .open()
            .and_then(|h| h.read_serial_number_string_ascii(&desc))
            .unwrap_or_else(|_| format!("bus{bus}-addr{address}"));

        let mut transport = RusbBulk::open_device(device).context("opening LCD device")?;
        transport
            .detach_and_configure("LCD")
            .context("configuring LCD device")?;

        Ok(Self {
            transport,
            bus,
            address,
            serial,
            initialized: false,
            screen: ScreenInfo::WIRELESS_LCD,
            firmware: None,
            new_lcd: None,
        })
    }

    pub fn bus(&self) -> u8 {
        self.bus
    }

    pub fn address(&self) -> u8 {
        self.address
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    pub fn screen_info(&self) -> &ScreenInfo {
        &self.screen
    }

    fn send_init(&mut self, builder: &mut PacketBuilder) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        debug!("LCD[bus {} addr {}] init sequence", self.bus, self.address);

        let mut buf = [0u8; 512];
        let check = builder.lcd_revision_header();
        self.transport.write_full(&check, LCD_WRITE_TIMEOUT)?;
        // Vendor CheckNewLcd checks after 10 ms and selects the legacy path without a reply.
        let length = match self
            .transport
            .read(&mut buf, std::time::Duration::from_millis(10))
        {
            Ok(length) => length,
            Err(lianli_transport::error::TransportError::Usb(rusb::Error::Timeout)) => 0,
            Err(error) => return Err(error.into()),
        };
        self.new_lcd = Some(uses_relative_startup_path(&buf[..length]));

        // Wireless LCDs use the vendor's fixed frame rate of 120.
        let fps = builder.frame_rate_header(120);
        self.transport.write(&fps, LCD_WRITE_TIMEOUT)?;
        let _ = self.transport.read(&mut buf, USB_TIMEOUT);

        let ver = builder.header(0, 0x0A, false);
        self.transport.write(&ver, LCD_WRITE_TIMEOUT)?;
        let n = self.transport.read(&mut buf, USB_TIMEOUT).unwrap_or(0);
        if n > 8 && buf[0] == 0x0a {
            let end = buf[8..n]
                .iter()
                .position(|&b| b == 0)
                .map(|p| 8 + p)
                .unwrap_or(n.min(40));
            let fw = String::from_utf8_lossy(&buf[8..end]).trim().to_string();
            if !fw.is_empty() {
                info!("SLV3/TLV2 LCD firmware: {fw}");
                self.firmware = Some(fw);
            }
        }

        self.initialized = true;
        Ok(())
    }

    /// Set LCD brightness (0-100). Uses legacy DES path opcode 0x0E.
    pub fn set_brightness(&mut self, builder: &mut PacketBuilder, brightness: u8) -> Result<()> {
        self.send_init(builder)?;
        let mapped = slv3_brightness_lut(brightness);
        let header = builder.brightness_header(mapped);
        self.transport.write(&header, LCD_WRITE_TIMEOUT)?;
        let mut buf = [0u8; 511];
        let _ = self.transport.read(&mut buf, USB_TIMEOUT);
        debug!("SLV3/TLV2 LCD brightness: {brightness} -> {mapped}");
        Ok(())
    }

    /// Reboot the LCD MCU. Uses legacy DES path opcode 0x0B.
    pub fn reboot(&mut self, builder: &mut PacketBuilder) -> Result<()> {
        let header = builder.header(0, 0x0B, false);
        self.transport.write(&header, LCD_WRITE_TIMEOUT)?;
        debug!("SLV3/TLV2 LCD reboot sent");
        Ok(())
    }

    pub fn firmware_str(&self) -> Option<&str> {
        self.firmware.as_deref()
    }

    pub fn startup_image_ready(&self) -> Result<()> {
        self.new_lcd.context(
            "LCD hardware revision is unknown; reconnect before uploading a startup image",
        )?;
        Ok(())
    }

    pub fn upload_startup_image(
        &mut self,
        jpeg: &[u8],
        stop: &std::sync::atomic::AtomicBool,
        transfer: &crate::startup_image::Transfer,
    ) -> Result<bool> {
        crate::startup_image::ensure_not_cancelled(stop)?;
        let new_path = self.new_lcd.context(
            "LCD hardware revision is unknown; reconnect before uploading a startup image",
        )?;
        let packet =
            crate::startup_image::packet(&mut PacketBuilder::new(), jpeg, new_path, true, false)?;
        crate::startup_image::ensure_not_cancelled(stop)?;
        lianli_transport::usb::with_teardown_io(std::time::Duration::from_secs(4), || {
            transfer.begin(stop)?;
            anyhow::ensure!(
                self.transport
                    .write(&packet, std::time::Duration::from_secs(3))
                    .context("Startup image transfer interrupted. Storage state is unknown")?
                    == packet.len(),
                "Startup image transfer incomplete. Storage state is unknown"
            );
            let mut reply = [0; 512];
            Ok(self
                .transport
                .read(&mut reply, std::time::Duration::from_secs(1))
                .is_ok_and(|length| length > 0))
        })
    }

    pub fn send_frame(&mut self, builder: &mut PacketBuilder, frame: &[u8]) -> Result<()> {
        if frame.len() > self.screen.max_payload {
            bail!(
                "frame payload {} exceeds LCD payload limit {}",
                frame.len(),
                self.screen.max_payload
            );
        }

        self.send_init(builder)?;

        let header = builder.header(frame.len(), 0x65, true);
        let mut packet = vec![0u8; 102_400];
        packet[..512].copy_from_slice(&header);
        packet[512..512 + frame.len()].copy_from_slice(frame);

        self.transport
            .write(&packet, LCD_WRITE_TIMEOUT)
            .context("writing LCD frame data")?;

        let mut buf = [0u8; 511];
        let _ = self.transport.read(&mut buf, USB_TIMEOUT);
        Ok(())
    }
}

fn uses_relative_startup_path(reply: &[u8]) -> bool {
    crate::startup_image::revision(reply).unwrap_or(false)
}

/// Brightness LUT for SLV3/TLV2 wireless LCD firmware. Maps 0–100 percent to
/// the expected byte via 5 anchor points with linear interpolation.
fn slv3_brightness_lut(value: u8) -> u8 {
    const ANCHORS: &[(u8, u8)] = &[(0, 0), (25, 10), (50, 30), (75, 40), (100, 100)];
    let v = value.min(100);
    if let Ok(idx) = ANCHORS.binary_search_by_key(&v, |&(in_v, _)| in_v) {
        return ANCHORS[idx].1;
    }
    let pos = ANCHORS
        .iter()
        .position(|&(in_v, _)| in_v > v)
        .unwrap_or(ANCHORS.len());
    let (lo_in, lo_out) = ANCHORS[pos - 1];
    let (hi_in, hi_out) = ANCHORS[pos];
    let span = (hi_in - lo_in) as u32;
    if span == 0 {
        return lo_out;
    }
    let num = (v - lo_in) as u32;
    let step_lo = (hi_out as u32).saturating_sub(lo_out as u32);
    lo_out.saturating_add(((num * step_lo + span / 2) / span) as u8)
}

#[cfg(test)]
mod tests {
    use super::slv3_brightness_lut;

    #[test]
    fn legacy_panels_without_revision_replies_use_the_absolute_boot_path() {
        assert!(!super::uses_relative_startup_path(&[]));
        assert!(!super::uses_relative_startup_path(&[0x80]));
        let mut reply = [0; 10];
        reply[0] = 0x80;
        assert!(!super::uses_relative_startup_path(&reply));
        reply[8] = 2;
        assert!(super::uses_relative_startup_path(&reply));
        reply[0] = 0x0a;
        assert!(!super::uses_relative_startup_path(&reply));
    }

    #[test]
    fn lut_anchors_match_vendor() {
        assert_eq!(slv3_brightness_lut(0), 0);
        assert_eq!(slv3_brightness_lut(25), 10);
        assert_eq!(slv3_brightness_lut(50), 30);
        assert_eq!(slv3_brightness_lut(75), 40);
        assert_eq!(slv3_brightness_lut(100), 100);
    }

    #[test]
    fn lut_interpolates_linearly() {
        // Between 25 -> 10 and 50 -> 30: midpoint ~37 -> 20
        assert_eq!(slv3_brightness_lut(37), 20);
        // Between 50 -> 30 and 75 -> 40: midpoint ~62 -> 35
        assert_eq!(slv3_brightness_lut(62), 35);
    }

    #[test]
    fn lut_clamps_above_100() {
        assert_eq!(slv3_brightness_lut(200), 100);
    }
}
