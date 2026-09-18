//! Configuration changes become visible only after successful persistence.

use super::EventSender;

use lianli_shared::config::LcdConfig;
use lianli_shared::fan::FanConfig;
use lianli_shared::ipc::IpcResponse;
use lianli_shared::rgb::RgbAppConfig;

use crate::ipc::{persist_and_notify, SharedState};

pub(super) fn validate_aio_modes(
    config: &lianli_shared::config::AppConfig,
    previous: Option<&lianli_shared::config::AppConfig>,
    devices: &[lianli_shared::ipc::DeviceInfo],
) -> Option<String> {
    for device in devices {
        let Some(aio) = config.aio.get(&device.device_id) else {
            continue;
        };
        let old = previous.and_then(|config| config.aio.get(&device.device_id));
        if unsupported_sync_change(
            &aio.pump_target_rpm,
            old.map(|aio| &aio.pump_target_rpm),
            device.pump_mb_sync_support,
        ) {
            return Some(format!(
                "{} does not support pump motherboard sync",
                device.name
            ));
        }
        for (slot, speed) in aio
            .fan_speeds
            .iter()
            .take(usize::from(device.fan_count.unwrap_or(0)))
            .enumerate()
        {
            if unsupported_sync_change(
                speed,
                old.map(|aio| &aio.fan_speeds[slot]),
                device.mb_sync_support,
            ) {
                return Some(format!(
                    "{} does not support fan motherboard sync",
                    device.name
                ));
            }
        }
    }
    None
}

fn unsupported_sync_change(
    next: &lianli_shared::fan::FanSpeed,
    previous: Option<&lianli_shared::fan::FanSpeed>,
    supported: bool,
) -> bool {
    next.is_mb_sync() && !supported && !previous.is_some_and(|speed| speed.is_mb_sync())
}

pub fn set_lcd_media(
    state: &SharedState,
    tx: EventSender,
    device_id: String,
    config: LcdConfig,
) -> IpcResponse {
    let mut state = state.lock();
    let mut app_config = state.config.clone().unwrap_or_default();
    if let Some(lcd) = app_config
        .lcds
        .iter_mut()
        .find(|l| l.device_id() == device_id)
    {
        *lcd = config;
    } else {
        app_config.lcds.push(config);
    }
    persist_and_notify(&mut state, &tx, "SetLcdMedia", app_config)
}

pub fn set_fan_config(state: &SharedState, tx: EventSender, config: FanConfig) -> IpcResponse {
    let mut state = state.lock();
    let mut app_config = state.config.clone().unwrap_or_default();
    app_config.fans = Some(config);
    persist_and_notify(&mut state, &tx, "SetFanConfig", app_config)
}

pub fn set_rgb_config(state: &SharedState, tx: EventSender, config: RgbAppConfig) -> IpcResponse {
    if let Some(response) = super::rgb::validate_config(state, &config) {
        return response;
    }
    let mut state = state.lock();
    let mut app_config = state.config.clone().unwrap_or_default();
    app_config.rgb = Some(config);
    persist_and_notify(&mut state, &tx, "SetRgbConfig", app_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::fan::FanSpeed;

    #[test]
    fn unavailable_sync_cannot_be_newly_selected_but_old_settings_remain_editable() {
        let mb = FanSpeed::Curve("__mb_sync__".into());
        let constant = FanSpeed::Constant(128);
        assert!(unsupported_sync_change(&mb, Some(&constant), false));
        assert!(unsupported_sync_change(&mb, None, false));
        assert!(!unsupported_sync_change(&mb, Some(&mb), false));
        assert!(!unsupported_sync_change(&constant, Some(&mb), false));
        assert!(!unsupported_sync_change(&mb, None, true));
        let mut config = lianli_shared::config::AppConfig::default();
        config.aio.insert(
            "offline".into(),
            lianli_shared::aio::AioConfig {
                pump_target_rpm: mb,
                ..Default::default()
            },
        );
        assert!(validate_aio_modes(&config, None, &[]).is_none());
    }
}
