//! Configuration changes become visible only after successful persistence.

use super::EventSender;

use lianli_shared::config::LcdConfig;
use lianli_shared::fan::FanConfig;
use lianli_shared::ipc::IpcResponse;
use lianli_shared::rgb::RgbAppConfig;

use crate::ipc::{persist_and_notify, SharedState};

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
