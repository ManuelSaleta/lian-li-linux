use super::DeviceKey;
use anyhow::{Context, Result};
use lianli_devices::turzx;

/// A single detected TURZX device from a bus scan.
#[derive(Debug, Clone)]
pub struct TurzxDeviceMatch {
    pub device: rusb::Device<rusb::GlobalContext>,
    pub pid: u16,
    pub key: DeviceKey,
}

/// Enumerate currently-attached TURZX panels on the USB bus.
pub fn enumerate_turzx() -> Result<Vec<TurzxDeviceMatch>> {
    let devices = rusb::devices().context("rusb::devices")?;
    let mut out = Vec::new();
    for device in devices.iter() {
        let desc = match device.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        let vid = desc.vendor_id();
        let pid = desc.product_id();
        if !turzx::is_turzx(vid, pid) {
            continue;
        }
        out.push(TurzxDeviceMatch {
            pid,
            key: (device.bus_number(), device.address()),
            device,
        });
    }
    Ok(out)
}
