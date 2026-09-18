use super::*;

impl WiredReceiverController {
    fn set_fan_duties(&self, duties: &[Option<u8>]) -> Result<()> {
        anyhow::ensure!(duties.len() <= 4, "too many receiver fan slots");
        if duties.iter().all(Option::is_none) {
            return Ok(());
        }
        let mut cached = self.fan_pwm.lock();
        if self.is_wireless.load(Ordering::Relaxed) {
            return Ok(());
        }
        let fan_count = *self.fan_count.lock() as usize;
        let right_attach = *self.is_inf_right_attach.lock();
        let previous = if duties.len() == 4 && duties.iter().all(Option::is_some) {
            [0; 4]
        } else {
            cached.context("receiver sibling duties unknown; set all fan duties first")?
        };
        let native = merge_fan_duties(self.params, previous, duties, fan_count, right_attach);
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_SET_FANS_PWM;
        tx[1..5].copy_from_slice(&native);
        *cached = None;
        let rx = self.send_and_read(&tx)?;
        validate_pwm_ack(&rx)?;
        *cached = Some(native);
        debug!("{}: SetFansPWM {:?}", self.params.name, &tx[1..5]);
        Ok(())
    }

    /// Enable/disable motherboard PWM sync. Sentinels all four fan ports
    /// to value 6, which the firmware interprets as "follow MB PWM header."
    pub fn set_mb_sync(&self, enabled: bool) -> Result<()> {
        let mut cached = self.fan_pwm.lock();
        if self.is_wireless.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_SET_FANS_PWM;
        if enabled {
            tx[1] = 6;
            tx[2] = 6;
            tx[3] = 6;
            tx[4] = 6;
        } else {
            return Ok(());
        }
        *cached = None;
        let rx = self.send_and_read(&tx)?;
        validate_pwm_ack(&rx)?;
        *cached = Some([6; 4]);
        debug!("{}: MB PWM sync = {enabled}", self.params.name);
        Ok(())
    }

    /// SelectedGroup (0x16) — group selection / keepalive.
    pub fn selected_group(&self) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_SELECTED_GROUP;
        self.send_and_read(&tx)?;
        Ok(())
    }

    /// SetLightSyncMB (0x14) — enable/disable motherboard RGB sync.
    pub fn set_light_sync_mb(&self, enable: bool) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_SET_LIGHT_SYNC_MB;
        tx[1] = if enable { 1 } else { 0 };
        let rx = self.send_and_read(&tx)?;
        if rx[0] != CMD_SET_LIGHT_SYNC_MB {
            warn!("SetLightSyncMB unexpected response: 0x{:02x}", rx[0]);
        }
        Ok(())
    }

    /// SaveOrClearConfig (0x15) — save current RGB to NVRAM, or clear.
    /// `save=true` writes to NVRAM; `save=false` clears saved config.
    pub fn save_or_clear_config(&self, save: bool) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_SAVE_OR_CLEAR;
        tx[1] = if save { 1 } else { 0 };
        let rx = self.send_and_read(&tx)?;
        if rx[0] != CMD_SAVE_OR_CLEAR {
            warn!("SaveOrClearConfig unexpected response: 0x{:02x}", rx[0]);
        }
        Ok(())
    }

    /// RebootLcd (0x17) — reboot the wired LCD group.
    pub fn reboot_lcd(&self) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_REBOOT_LCD;
        let rx = self.send_and_read(&tx)?;
        if rx[0] != CMD_REBOOT_LCD && rx[0] != CMD_SAVE_OR_CLEAR {
            warn!("RebootLcd unexpected response: 0x{:02x}", rx[0]);
        }
        Ok(())
    }

    /// FanAndFixedData (0x26) — per-fan theme/data/brightness push.
    /// `fans_data` is up to 62 bytes of per-fan configuration.
    pub fn update_fans_theme_and_data(&self, fans_data: &[u8]) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_FAN_AND_FIXED_DATA;
        let len = fans_data.len().min(62);
        tx[2..2 + len].copy_from_slice(&fans_data[..len]);
        let rx = self.send_and_read(&tx)?;
        if rx[0] != CMD_FAN_THEME_COLOR && rx[0] != CMD_FAN_AND_FIXED_DATA {
            warn!("FanAndFixedData unexpected response: 0x{:02x}", rx[0]);
        }
        Ok(())
    }

    /// FanThemeColor (0x27) — per-fan color palette.
    /// `colors` is the RGB data; `fan_index` selects which fan (0-3).
    pub fn update_fans_color(&self, colors: &[[u8; 3]], fan_index: u8) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_FAN_THEME_COLOR;
        tx[1] = if fan_index > 1 { 1 } else { 0 };
        let start = 2 + (fan_index as usize % 2) * 19;
        let mut num = start;
        for c in colors.iter().take(6) {
            if num + 3 > PACKET_SIZE {
                break;
            }
            tx[num] = c[0];
            tx[num + 1] = c[1];
            tx[num + 2] = c[2];
            num += 3;
        }
        let mut nonce = self.color_nonce.lock();
        *nonce = nonce.wrapping_add(1).max(1);
        tx[num] = *nonce;
        tx[num + 1] = nonce.wrapping_add(1).max(1);
        drop(nonce);
        let rx = self.send_and_read(&tx)?;
        if rx[0] != CMD_FAN_THEME_COLOR {
            warn!("FanThemeColor unexpected response: 0x{:02x}", rx[0]);
        }
        Ok(())
    }

    /// WirelessThemeSwitch (0x29) — toggle embedded theme vs USB frames.
    /// Bit `i` (0..3) = fan `i` uses embedded/wireless theme.
    pub fn update_wireless_theme_switch(&self, mask: u8) -> Result<()> {
        let mut tx = [0u8; PACKET_SIZE];
        tx[0] = CMD_WIRELESS_THEME_SWITCH;
        tx[1] = mask;
        let rx = self.send_and_read(&tx)?;
        if rx[0] != CMD_WIRELESS_THEME_SWITCH {
            warn!("WirelessThemeSwitch unexpected response: 0x{:02x}", rx[0]);
        }
        Ok(())
    }
}

impl FanDevice for WiredReceiverController {
    fn set_fan_speed(&self, slot: u8, duty: u8) -> Result<()> {
        anyhow::ensure!(
            slot < self.fan_slot_count(),
            "receiver fan slot out of range"
        );
        let mut duties = [None; 4];
        duties[slot as usize] = Some(duty);
        self.set_fan_duties(&duties)
    }

    fn set_fan_speeds(&self, duties: &[u8]) -> Result<()> {
        self.set_fan_duties(&duties.iter().copied().map(Some).collect::<Vec<_>>())
    }

    fn set_selected_fan_speeds(&self, duties: &[Option<u8>]) -> Result<()> {
        anyhow::ensure!(
            duties
                .iter()
                .skip(self.fan_slot_count() as usize)
                .all(Option::is_none),
            "receiver fan slot out of range"
        );
        self.set_fan_duties(duties)
    }

    fn read_fan_rpm(&self) -> Result<Vec<u16>> {
        if self.is_wireless.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let status = self.get_info()?;
        Ok(status.fan_rpm.to_vec())
    }

    fn fan_slot_count(&self) -> u8 {
        *self.fan_count.lock()
    }

    fn supports_mb_sync(&self) -> bool {
        true
    }

    fn set_mb_rpm_sync(&self, _port: u8, sync: bool) -> Result<()> {
        self.set_mb_sync(sync)
    }

    fn wireless_link_mac(&self) -> Option<[u8; 6]> {
        *self.mac.lock()
    }

    fn set_wireless_bound(&self, bound: bool) {
        let mut cached = self.fan_pwm.lock();
        if self.is_wireless.load(Ordering::Relaxed) != bound {
            *cached = None;
        }
        self.is_wireless.store(bound, Ordering::Relaxed);
    }
}

fn validate_pwm_ack(response: &[u8]) -> Result<()> {
    anyhow::ensure!(
        response.len() >= 2 && response[0] == CMD_SET_FANS_PWM && response[1] == 0,
        "receiver rejected PWM command: {:02x?}",
        response
    );
    Ok(())
}

fn merge_fan_duties(
    params: ReceiverParams,
    mut native: [u8; 4],
    duties: &[Option<u8>],
    fan_count: usize,
    right_attach: bool,
) -> [u8; 4] {
    let mut selected = [None; 4];
    selected[..duties.len()].copy_from_slice(duties);
    if right_attach {
        reverse_fan_slots(&mut selected, fan_count);
    }
    let floor = (u16::from(params.pwm_floor) * 255 + 50) / 100;
    for (target, duty) in native.iter_mut().zip(selected) {
        if let Some(duty) = duty {
            *target = if duty == 0 {
                params.pwm_zero
            } else {
                duty.max(floor as u8)
            };
        }
    }
    native
}

/// Reverse per-fan slot ordering for SL-INF right-attach daisy-chains.
fn reverse_fan_slots<T: Copy>(slots: &mut [T; 4], fan_count: usize) {
    let n = fan_count.min(4);
    if n > 1 {
        slots[..n].reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwm_ack_requires_opcode_and_success_status() {
        for response in [&[][..], &[0x13], &[0x12, 0], &[0x13, 1]] {
            assert!(validate_pwm_ack(response).is_err());
        }
        assert!(validate_pwm_ack(&[0x13, 0]).is_ok());
    }

    #[test]
    fn raw_duty_is_scaled_once_and_zero_has_its_native_sentinel() {
        for (pid, floor, zero) in [
            (0x0101, 28, 5),
            (0x0102, 28, 5),
            (0x0103, 26, 5),
            (0x0104, 26, 5),
            (0x0105, 20, 1),
            (0x0106, 36, 5),
            (0x0107, 26, 5),
        ] {
            let params = ReceiverParams::from_pid(pid).unwrap();
            for (raw, expected) in [
                (0, zero),
                (1, floor),
                (5, floor),
                (6, floor),
                (127, 127),
                (128, 128),
                (255, 255),
            ] {
                assert_eq!(
                    merge_fan_duties(params, [0; 4], &[Some(raw); 4], 4, false),
                    [expected; 4]
                );
            }
        }
    }

    #[test]
    fn selected_slot_preserves_siblings_and_right_attach_order() {
        let params = ReceiverParams::from_pid(0x0103).unwrap();
        let previous = [6, 100, 150, 5];
        assert_eq!(
            merge_fan_duties(params, previous, &[Some(128)], 3, false),
            [128, 100, 150, 5]
        );
        let next = merge_fan_duties(params, previous, &[Some(128)], 3, true);
        assert_eq!(next, [6, 100, 128, 5]);
        assert_eq!(
            merge_fan_duties(params, next, &[None, Some(0)], 3, true),
            [6, 5, 128, 5]
        );
    }
}
