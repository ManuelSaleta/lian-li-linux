use super::protocol::{
    A_HEADER_LEN, A_PACKET_SIZE, CMD_SET_FAN_LIGHT, CMD_SET_PUMP_LIGHT, FAN_LED_COUNT, REPORT_ID_A,
};
use super::{AioLcdVariant, HydroShiftLcdController};
use crate::registry::SharedHid;
use crate::traits::RgbDevice;
use anyhow::{bail, Context, Result};
use lianli_shared::rgb::{RgbEffect, RgbMode, RgbScope, RgbZoneInfo};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tracing::{debug, info};

const GALAHAD_FAN_LED_CONTROL: lianli_shared::rgb::RgbLedCountControl =
    lianli_shared::rgb::RgbLedCountControl {
        zone: 1,
        min: 8,
        max: 50,
        default: 24,
    };

pub struct AioLcdRgbController {
    device: SharedHid,
    variant: AioLcdVariant,
    fan_led_count: AtomicU16,
    controller: Arc<HydroShiftLcdController>,
    pending: Mutex<Vec<(u8, Vec<u8>)>>,
}

impl AioLcdRgbController {
    pub(crate) fn new(
        device: SharedHid,
        pid: u16,
        controller: Arc<HydroShiftLcdController>,
    ) -> Result<Self> {
        let variant = AioLcdVariant::from_pid(pid)
            .ok_or_else(|| anyhow::anyhow!("Unknown AIO LCD PID: {pid:#06x}"))?;
        info!("Opened {} RGB controller", variant.name());
        Ok(Self {
            device,
            variant,
            fan_led_count: AtomicU16::new(FAN_LED_COUNT),
            controller,
            pending: Mutex::new(Vec::new()),
        })
    }

    fn set_pump_light(&self, effect: &RgbEffect, source_mcu: bool) -> Result<()> {
        let scope = match effect.scope {
            RgbScope::Inner => 0u8,
            RgbScope::Outer => 1,
            _ => 2,
        };
        let mode_byte = effect.mode.to_hydroshift_lcd_mode_byte().unwrap_or(3);
        let mut payload = [0u8; 19];
        payload[0] = scope;
        payload[1] = mode_byte;
        payload[2] = lianli_shared::rgb::brightness_scale(effect.brightness);
        payload[3] = lianli_shared::rgb::brightness_scale(effect.speed);
        for (i, color) in effect.colors.iter().take(4).enumerate() {
            let offset = 4 + i * 3;
            payload[offset] = color[0];
            payload[offset + 1] = color[1];
            payload[offset + 2] = color[2];
        }
        payload[16] = effect.direction.to_tl_byte();
        payload[17] = (effect.disabled || effect.mode == RgbMode::Off) as u8;
        payload[18] = if source_mcu { 0 } else { 1 };
        self.send_rgb_command(CMD_SET_PUMP_LIGHT, &payload)?;
        debug!("Set pump light: mode={mode_byte} scope={scope}");
        Ok(())
    }

    fn set_fan_light(
        &self,
        effect: &RgbEffect,
        source_mcu: bool,
        sync_to_pump: bool,
    ) -> Result<()> {
        let payload = Self::fan_light_payload(
            effect,
            source_mcu,
            sync_to_pump,
            self.fan_led_count.load(Ordering::Relaxed),
        );
        self.send_rgb_command(CMD_SET_FAN_LIGHT, &payload)
    }

    fn fan_light_payload(
        effect: &RgbEffect,
        source_mcu: bool,
        sync_to_pump: bool,
        led_count: u16,
    ) -> [u8; 20] {
        let mode_byte = effect.mode.to_hydroshift_lcd_mode_byte().unwrap_or(3);
        let mut payload = [0u8; 20];
        payload[0] = mode_byte;
        payload[1] = lianli_shared::rgb::brightness_scale(effect.brightness);
        payload[2] = lianli_shared::rgb::brightness_scale(effect.speed);
        for (i, color) in effect.colors.iter().take(4).enumerate() {
            let offset = 3 + i * 3;
            payload[offset] = color[0];
            payload[offset + 1] = color[1];
            payload[offset + 2] = color[2];
        }
        payload[15] = effect.direction.to_tl_byte();
        payload[16] = (effect.disabled || effect.mode == RgbMode::Off) as u8;
        payload[17] = if source_mcu { 0 } else { 1 };
        payload[18] = sync_to_pump as u8;
        payload[19] = led_count as u8;
        payload
    }

    fn send_rgb_command(&self, cmd: u8, data: &[u8]) -> Result<()> {
        let mut pending = self.pending.lock();
        if !self.controller.initialization_ready()? {
            defer_rgb_command(&mut pending, cmd, data);
            return Ok(());
        }
        self.flush_commands(&mut pending)?;
        self.write_rgb_command(cmd, data)
    }

    pub(super) fn flush_pending(&self) -> Result<()> {
        let mut pending = self.pending.lock();
        if self.controller.initialization_ready()? {
            self.flush_commands(&mut pending)?;
        }
        Ok(())
    }

    fn flush_commands(&self, pending: &mut Vec<(u8, Vec<u8>)>) -> Result<()> {
        while let Some((cmd, data)) = pending.first() {
            self.write_rgb_command(*cmd, data)?;
            pending.remove(0);
        }
        Ok(())
    }

    fn write_rgb_command(&self, cmd: u8, data: &[u8]) -> Result<()> {
        let max_payload = A_PACKET_SIZE - A_HEADER_LEN;
        if data.len() > max_payload {
            bail!(
                "AIO LCD RGB: command {cmd:#04x} payload too large ({} > {max_payload})",
                data.len()
            );
        }
        let mut pkt = [0u8; A_PACKET_SIZE];
        pkt[0] = REPORT_ID_A;
        pkt[1] = cmd;
        pkt[5] = data.len() as u8;
        pkt[A_HEADER_LEN..A_HEADER_LEN + data.len()].copy_from_slice(data);

        let mut dev = self.device.lock();
        dev.write(&pkt).context("AIO LCD RGB: write")?;
        Ok(())
    }
}

fn defer_rgb_command(pending: &mut Vec<(u8, Vec<u8>)>, cmd: u8, data: &[u8]) {
    pending.retain(|(old_cmd, old_data)| {
        *old_cmd != cmd || (cmd == CMD_SET_PUMP_LIGHT && data[0] != 2 && old_data[0] != data[0])
    });
    pending.push((cmd, data.to_vec()));
}

impl RgbDevice for AioLcdRgbController {
    fn fan_led_count_control(&self) -> Option<lianli_shared::rgb::RgbLedCountControl> {
        matches!(
            self.variant,
            AioLcdVariant::Galahad2Lcd | AioLcdVariant::Galahad2Vision
        )
        .then_some(GALAHAD_FAN_LED_CONTROL)
    }

    fn configure_fan_led_count(&self, count: Option<u16>) -> Result<bool> {
        let control = self
            .fan_led_count_control()
            .context("Adjustable fan LED count is unsupported")?;
        let count = control.resolve(count).map_err(anyhow::Error::msg)?;
        Ok(self.fan_led_count.swap(count, Ordering::Relaxed) != count)
    }

    fn device_name(&self) -> String {
        format!("{} AIO", self.variant.name())
    }

    fn supported_modes(&self) -> Vec<RgbMode> {
        if matches!(self.variant, super::AioLcdVariant::Galahad2Vision) {
            vec![
                RgbMode::Off,
                RgbMode::Static,
                RgbMode::Rainbow,
                RgbMode::RainbowMorph,
                RgbMode::Breathing,
                RgbMode::Runway,
                RgbMode::Meteor,
            ]
        } else {
            vec![
                RgbMode::Off,
                RgbMode::Static,
                RgbMode::Rainbow,
                RgbMode::RainbowMorph,
                RgbMode::Breathing,
                RgbMode::Runway,
                RgbMode::Meteor,
                RgbMode::TickerTape,
                RgbMode::Fluctuation,
                RgbMode::Transmit,
                RgbMode::ColorfulStarryNight,
                RgbMode::StaticStarryNight,
                RgbMode::Voice,
                RgbMode::BigBang,
                RgbMode::Burst,
                RgbMode::ColorsMorph,
                RgbMode::Bounce,
            ]
        }
    }

    fn zone_info(&self) -> Vec<RgbZoneInfo> {
        zones(self.variant, self.fan_led_count.load(Ordering::Relaxed))
    }

    fn set_zone_effect(&self, zone: u8, effect: &RgbEffect) -> Result<()> {
        if self.variant.has_pump_rgb() {
            match zone {
                0 => self.set_pump_light(effect, true),
                1 => self.set_fan_light(effect, true, false),
                _ => bail!("{}: zone {zone} out of range (0-1)", self.variant.name()),
            }
        } else {
            match zone {
                0 => self.set_fan_light(effect, true, false),
                _ => bail!("{}: zone {zone} out of range (0)", self.variant.name()),
            }
        }
    }

    fn supported_scopes(&self) -> Vec<Vec<RgbScope>> {
        if self.variant.has_pump_rgb() {
            vec![
                vec![RgbScope::All, RgbScope::Inner, RgbScope::Outer],
                vec![],
            ]
        } else {
            vec![]
        }
    }

    fn supports_mb_rgb_sync(&self) -> bool {
        true
    }

    fn set_mb_rgb_sync(&self, enabled: bool) -> Result<()> {
        let source_mcu = !enabled;
        let dummy = RgbEffect::default();
        if self.variant.has_pump_rgb() {
            self.set_pump_light(&dummy, source_mcu)?;
        }
        self.set_fan_light(&dummy, source_mcu, false)?;
        debug!("Set MB RGB sync: enabled={enabled}");
        Ok(())
    }
}

fn zones(variant: AioLcdVariant, fan_led_count: u16) -> Vec<RgbZoneInfo> {
    if variant.has_pump_rgb() {
        vec![
            RgbZoneInfo {
                name: "Pump Head".to_string(),
                led_count: 12,
            },
            RgbZoneInfo {
                name: "Fans".to_string(),
                led_count: fan_led_count,
            },
        ]
    } else {
        vec![RgbZoneInfo {
            name: "Fans".to_string(),
            led_count: FAN_LED_COUNT,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_rgb_keeps_latest_settings_and_scope_order_bounded() {
        let mut pending = Vec::new();
        for value in 0..100 {
            defer_rgb_command(&mut pending, CMD_SET_FAN_LIGHT, &[value]);
            for scope in [2, 0, 1] {
                defer_rgb_command(&mut pending, CMD_SET_PUMP_LIGHT, &[scope, value]);
            }
            assert_eq!(pending.len(), 4);
        }
        assert_eq!(pending[0], (CMD_SET_FAN_LIGHT, vec![99]));
        assert_eq!(pending[1], (CMD_SET_PUMP_LIGHT, vec![2, 99]));
        assert_eq!(pending[2], (CMD_SET_PUMP_LIGHT, vec![0, 99]));
        assert_eq!(pending[3], (CMD_SET_PUMP_LIGHT, vec![1, 99]));
        defer_rgb_command(&mut pending, CMD_SET_PUMP_LIGHT, &[2, 100]);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[1], (CMD_SET_PUMP_LIGHT, vec![2, 100]));
    }

    #[test]
    fn galahad_fan_led_count_payload_and_bounds() {
        assert_eq!(GALAHAD_FAN_LED_CONTROL.resolve(None).unwrap(), 24);
        for count in [8, 24, 50] {
            let count = GALAHAD_FAN_LED_CONTROL.resolve(Some(count)).unwrap();
            for source_mcu in [false, true] {
                let payload = AioLcdRgbController::fan_light_payload(
                    &RgbEffect::default(),
                    source_mcu,
                    false,
                    count,
                );
                assert_eq!(payload.len(), 20);
                assert_eq!(payload[17], u8::from(!source_mcu));
                assert_eq!(payload[18], 0);
                assert_eq!(payload[19], count as u8);
            }
        }
        for count in [0, 7, 51, 256, u16::MAX] {
            assert!(GALAHAD_FAN_LED_CONTROL.resolve(Some(count)).is_err());
        }
    }

    #[test]
    fn galahad_pump_led_count_matches_both_vendor_dispatch_ids() {
        for pid in [0x7391, 0x7395] {
            let variant = AioLcdVariant::from_pid(pid).unwrap();
            let layout = zones(variant, 50);
            assert!(variant.has_pump_rgb());
            assert_eq!(layout.len(), 2);
            assert_eq!(layout[0].led_count, 12);
            assert_eq!(layout[1].led_count, 50);
        }
        for pid in [0x7398, 0x7399, 0x739a] {
            let variant = AioLcdVariant::from_pid(pid).unwrap();
            assert!(!variant.has_pump_rgb());
            let layout = zones(variant, 50);
            assert_eq!(layout.len(), 1);
            assert_eq!(layout[0].led_count, 24);
        }
    }
}
