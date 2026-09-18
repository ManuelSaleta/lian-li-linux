//! LCD IPC handlers: `SwitchDisplayMode`, `RenderTemplatePreview`.

use super::EventSender;

use lianli_media::CustomAsset;
use lianli_shared::ipc::IpcResponse;
use lianli_shared::screen::ScreenInfo;

use crate::ipc::SharedState;
use crate::service::DaemonEvent;

pub fn upload_startup_image(
    state: &SharedState,
    tx: &EventSender,
    device_id: String,
    encoded: String,
) -> IpcResponse {
    use base64::Engine;
    use lianli_shared::startup_image::{StartupImageState, StartupImageStatus};
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let capabilities = {
        let state = state.lock();
        state
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .and_then(|device| lianli_shared::startup_image::capabilities(device.family))
    };
    let Some(capabilities) = capabilities else {
        return IpcResponse::error(
            "Startup image upload is not supported for this attached device",
        );
    };
    if encoded.len() > capabilities.max_jpeg_bytes.div_ceil(3) * 4 {
        return IpcResponse::error("Startup image exceeds the device payload limit");
    }
    let jpeg = match base64::engine::general_purpose::STANDARD.decode(encoded) {
        Ok(jpeg) if !jpeg.is_empty() && jpeg.len() <= capabilities.max_jpeg_bytes => jpeg,
        _ => return IpcResponse::error("Invalid or oversized startup JPEG"),
    };
    let mut state = state.lock();
    if state
        .startup_image
        .as_ref()
        .is_some_and(|job| job.status.is_pending())
    {
        return IpcResponse::error("Another startup image upload is still running");
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(AtomicBool::new(false));
    state.startup_image = Some(StartupImageStatus {
        id,
        device_id: device_id.clone(),
        status: StartupImageState::Pending,
    });
    state.startup_image_cancel = Some(cancel.clone());
    if tx
        .send(DaemonEvent::UploadStartupImage {
            id,
            device_id,
            jpeg,
            capabilities,
            cancel,
        })
        .is_err()
    {
        state.startup_image = None;
        state.startup_image_cancel = None;
        return IpcResponse::error("Daemon stopped; startup image was not queued");
    }
    IpcResponse::ok(serde_json::json!({ "id": id }))
}

pub fn cancel_startup_image(state: &SharedState, id: u64) -> IpcResponse {
    let state = state.lock();
    if state.startup_image.as_ref().is_none_or(|job| job.id != id) {
        return IpcResponse::error("Startup image job is no longer current");
    }
    if let Some(cancel) = &state.startup_image_cancel {
        cancel.store(true, std::sync::atomic::Ordering::Release);
    }
    IpcResponse::ok(serde_json::json!({"cancel_requested": true}))
}

pub fn retry_desktop(
    state: &SharedState,
    tx: &EventSender,
    bus: u8,
    address: u8,
    product_id: u16,
) -> IpcResponse {
    let failed = state.lock().telemetry.desktop_streams.iter().any(|stream| {
        stream.bus == bus
            && stream.address == address
            && stream.product_id == product_id
            && stream.state == lianli_shared::ipc::DesktopStreamState::Failed
    });
    if !failed {
        return IpcResponse::error(
            "The selected desktop display is no longer failed. Refresh its status.",
        );
    }
    match tx.send(DaemonEvent::RetryDesktopDisplay {
        bus,
        address,
        product_id,
    }) {
        Ok(()) => IpcResponse::ok(serde_json::json!({"accepted": true})),
        Err(_) => IpcResponse::error("The daemon is stopping. Retry was not queued"),
    }
}

pub fn switch_display_mode(state: &SharedState, tx: EventSender, device_id: String) -> IpcResponse {
    let (family, pid) = {
        let state = state.lock();
        match state.devices.iter().find(|d| d.device_id == device_id) {
            Some(d) => (Some(d.family), d.pid),
            None => (None, 0),
        }
    };
    match family {
        Some(f) if f.is_desktop_mode() => {
            if pid == 0 {
                return IpcResponse::error("device PID not available");
            }
            tracing::info!("LCD mode switch requested for {device_id}");
            if tx
                .send(DaemonEvent::DisplaySwitchToLcd { device_id, pid })
                .is_err()
            {
                return IpcResponse::error("The daemon is stopping. Mode switch was not queued");
            }
            IpcResponse::ok(serde_json::json!({
                "accepted": true,
                "message": "LCD mode switch queued. Wait for the device to reconnect."
            }))
        }
        Some(f) if f.supports_display_mode_switch() => {
            tracing::info!("Desktop mode switch requested for {device_id}");
            if tx.send(DaemonEvent::DisplaySwitch { device_id }).is_err() {
                return IpcResponse::error("The daemon is stopping. Mode switch was not queued");
            }
            IpcResponse::ok(serde_json::json!({
                "accepted": true,
                "message": "Desktop mode switch queued. Wait for the device to reconnect."
            }))
        }
        Some(_) => IpcResponse::error("device does not support display mode switching"),
        None => IpcResponse::error(format!("device not found: {device_id}")),
    }
}

pub fn render_template_preview(
    template: lianli_shared::template::LcdTemplate,
    width: u32,
    height: u32,
    hardware_video: bool,
    catalog_runtime: &crate::catalog_references::RuntimeReferences,
) -> IpcResponse {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let usage = match catalog_runtime.enter(|| {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Catalog review is busy. Retry this template preview shortly"
        );
        Ok(())
    }) {
        Ok(usage) => usage,
        Err(error) => return IpcResponse::error(error.to_string()),
    };
    let dependencies = lianli_shared::media_dependencies::template_dependencies(&template);
    usage.record(&dependencies);
    let preview_screen = ScreenInfo {
        width,
        height,
        max_fps: 30,
        jpeg_quality: 90,
        max_payload: 4 * 1024 * 1024,
        h264: false,
        needs_keepalive: false,
        png: false,
        play_count: 0,
    };
    let all_sensors = lianli_shared::sensors::enumerate_sensors();
    let response = match CustomAsset::new(
        &template,
        0.0,
        &preview_screen,
        &all_sensors,
        false,
        30.0,
        hardware_video,
    ) {
        Ok(asset) => {
            asset.seed_preview_history();
            match asset.render_frame(true) {
                Ok(Some(frame)) => IpcResponse::ok(serde_json::json!({
                    "jpeg_base64": super::server::base64_encode(&frame.data),
                })),
                Ok(None) => {
                    let blank = asset.blank_frame();
                    IpcResponse::ok(serde_json::json!({
                        "jpeg_base64": super::server::base64_encode(&blank.data),
                    }))
                }
                Err(e) => IpcResponse::error(format!("preview render failed: {e}")),
            }
        }
        Err(e) => IpcResponse::error(format!("preview asset creation failed: {e}")),
    };
    usage.record(&dependencies);
    response
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_requests_are_bounded_exclusive_and_cancel_by_job_identity() {
        use base64::Engine;
        let directory = tempfile::tempdir().unwrap();
        let mut state = super::super::DaemonState::new(directory.path().join("config.json"));
        state.devices.push(serde_json::from_value(serde_json::json!({
            "device_id": "lcd", "family": "Tlv2Lcd", "name": "Test", "vid": 0x1cbe, "pid": 0x0006,
            "has_lcd": true, "has_fan": false, "has_pump": false, "has_rgb": false, "mb_sync_support": false
        })).unwrap());
        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        let (sender, receiver) = std::sync::mpsc::channel();
        let sender: EventSender = sender.into();
        assert!(matches!(
            upload_startup_image(&state, &sender, "lcd".into(), "x".repeat(1_500_000)),
            IpcResponse::Error { .. }
        ));
        let encoded = base64::engine::general_purpose::STANDARD.encode([1, 2, 3]);
        assert!(matches!(
            upload_startup_image(&state, &sender, "lcd".into(), encoded.clone()),
            IpcResponse::Ok { .. }
        ));
        let event = receiver.recv().unwrap();
        let DaemonEvent::UploadStartupImage { id, cancel, .. } = event else {
            panic!("wrong event")
        };
        assert!(matches!(
            upload_startup_image(&state, &sender, "lcd".into(), encoded),
            IpcResponse::Error { .. }
        ));
        assert!(matches!(
            cancel_startup_image(&state, id + 1),
            IpcResponse::Error { .. }
        ));
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(matches!(
            cancel_startup_image(&state, id),
            IpcResponse::Ok { .. }
        ));
        assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
    }
    use super::*;

    #[test]
    fn unsupported_startup_uploads_are_rejected_without_creating_a_job() {
        use base64::Engine;
        use lianli_shared::device_id::DeviceFamily::*;
        let directory = tempfile::tempdir().unwrap();
        let mut state = super::super::DaemonState::new(directory.path().join("config.json"));
        let families = [
            UniversalScreen,
            Lancool207,
            Vision9p2,
            HydroShift2Lcd,
            HydroShift2OledCurveLcd,
        ];
        for family in families {
            state.devices.push(serde_json::from_value(serde_json::json!({
                "device_id": format!("{family:?}"), "family": family, "name": "Test", "vid": 0x1cbe, "pid": 0xa088,
                "has_lcd": true, "has_fan": false, "has_pump": false, "has_rgb": false, "mb_sync_support": false,
                "startup_image": {"width":480,"height":1920,"max_jpeg_bytes":1048576}
            })).unwrap());
        }
        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        let (sender, receiver) = std::sync::mpsc::channel();
        let sender: EventSender = sender.into();
        for family in families {
            let response = upload_startup_image(
                &state,
                &sender,
                format!("{family:?}"),
                base64::engine::general_purpose::STANDARD.encode([1, 2, 3]),
            );
            assert!(matches!(response, IpcResponse::Error { .. }), "{family:?}");
            assert!(receiver.try_recv().is_err());
            let state = state.lock();
            assert!(state.startup_image.is_none());
            assert!(state.startup_image_cancel.is_none());
        }
    }

    #[test]
    fn mode_switch_requires_delivery_without_a_capture_worker() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = super::super::DaemonState::new(directory.path().join("config.json"));
        state.devices.push(
            serde_json::from_value(serde_json::json!({
                "device_id": "desktop-test", "family": "UniversalScreenDesktop",
                "name": "Test", "vid": 0x1a86, "pid": 0xad21,
                "has_lcd": false, "has_fan": false, "has_pump": false,
                "has_rgb": false, "mb_sync_support": false
            }))
            .unwrap(),
        );
        let state = std::sync::Arc::new(parking_lot::Mutex::new(state));
        let (sender, receiver) = std::sync::mpsc::channel();
        let response = switch_display_mode(&state, sender.clone().into(), "desktop-test".into());
        assert!(matches!(response, IpcResponse::Ok { data } if data["accepted"] == true));
        assert!(
            matches!(receiver.try_recv().unwrap(), DaemonEvent::DisplaySwitchToLcd { device_id, pid: 0xad21 } if device_id == "desktop-test")
        );
        drop(receiver);
        assert!(matches!(
            switch_display_mode(&state, sender.into(), "desktop-test".into()),
            IpcResponse::Error { message } if message.contains("not queued")
        ));
    }
}
