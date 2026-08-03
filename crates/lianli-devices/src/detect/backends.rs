use super::enumerate::find_usb_device;
use super::DetectedDevice;
use anyhow::Result;
use lianli_shared::config::HidBackend;
use lianli_shared::device_id::DeviceFamily;
use lianli_transport::{HidTransport, HidrawTransport, RusbBulk, RusbHid, RusbHidReopener};
use parking_lot::Mutex;
use rusb::{Device, GlobalContext};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

type SharedHid = Arc<Mutex<Box<dyn HidTransport>>>;

fn make_rusb_reopener(
    vid: u16,
    pid: u16,
    bus: u8,
    port_numbers: Vec<u8>,
    usage_page: Option<u16>,
) -> RusbHidReopener {
    Arc::new(move || {
        let usb_dev = rusb::devices()
            .map_err(|e| anyhow::anyhow!("rusb devices: {e}"))?
            .iter()
            .find(|d| {
                d.bus_number() == bus
                    && d.port_numbers().ok().as_deref() == Some(&port_numbers[..])
                    && d.device_descriptor()
                        .map(|desc| desc.vendor_id() == vid && desc.product_id() == pid)
                        .unwrap_or(false)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "USB device {vid:04x}:{pid:04x} at {bus}-{:?} not enumerable on reopen",
                    port_numbers
                )
            })?;
        let transport = RusbHid::open_by_usage_with_reopener(
            usb_dev,
            usage_page,
            make_rusb_reopener(vid, pid, bus, port_numbers.clone(), usage_page),
        )
        .map_err(|e| anyhow::anyhow!("rusb hid open: {e}"))?;
        Ok(transport)
    })
}

fn try_open_with_retry<T>(
    usb_device: Option<&Device<GlobalContext>>,
    label: &str,
    mut open_fn: impl FnMut() -> Result<T>,
) -> Result<T> {
    const MAX_RETRIES: u32 = 3;
    const RESET_AT: u32 = 2;
    for attempt in 0..=MAX_RETRIES {
        match open_fn() {
            Ok(t) => return Ok(t),
            Err(e) if attempt < MAX_RETRIES => {
                if attempt == RESET_AT {
                    if let Some(usb_dev) = usb_device {
                        warn!(
                            "{label}: open attempt {} failed: {e}, resetting USB device",
                            attempt + 1
                        );
                        let _ = RusbHid::reset_usb_device(usb_dev);
                        std::thread::sleep(Duration::from_secs(3));
                    } else {
                        return Err(e.context(format!(
                            "{label}: failed and no USB device available for reset"
                        )));
                    }
                } else {
                    warn!(
                        "{label}: open attempt {} failed: {e}, retrying",
                        attempt + 1
                    );
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            Err(e) => {
                return Err(e.context(format!(
                    "{label}: failed after {} attempts",
                    MAX_RETRIES + 1
                )));
            }
        }
    }
    unreachable!()
}

fn open_rusb_with_retry<T>(
    usb_device: &Device<GlobalContext>,
    open_fn: impl FnMut() -> Result<T>,
) -> Result<T> {
    try_open_with_retry(Some(usb_device), "rusb open", open_fn)
}

fn usb_topology_string(bus: u8, port_numbers: &[u8]) -> String {
    format!(
        "{}-{}",
        bus,
        port_numbers
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(".")
    )
}

pub fn hidraw_path_for_usb_topology(bus: u8, port_numbers: &[u8]) -> Option<std::ffi::CString> {
    if port_numbers.is_empty() {
        return None;
    }
    let topology = usb_topology_string(bus, port_numbers);
    let needles = [format!("/{topology}/"), format!("/{topology}:")];
    let class_dir = std::path::Path::new("/sys/class/hidraw");
    for entry in std::fs::read_dir(class_dir).ok()?.flatten() {
        let Ok(resolved) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        let resolved_str = resolved.to_string_lossy();
        if needles.iter().any(|n| resolved_str.contains(n.as_str())) {
            let name = entry.file_name();
            let name = name.to_str()?;
            return std::ffi::CString::new(format!("/dev/{name}")).ok();
        }
    }
    None
}

fn open_hidraw_device(
    vid: u16,
    pid: u16,
    bus: u8,
    port_numbers: &[u8],
    usage_page: Option<u16>,
) -> Result<hidapi::HidDevice> {
    let api = hidapi::HidApi::new()
        .map_err(|e| anyhow::anyhow!("HidApi init: {e}"))?;

    let candidates: Vec<_> = api
        .device_list()
        .filter(|info| {
            info.vendor_id() == vid
                && info.product_id() == pid
                && usage_page.map_or(true, |up| info.usage_page() == up)
        })
        .collect();

    if candidates.is_empty() {
        debug!(
            "No hidraw device matching {vid:04x}:{pid:04x} usage_page={usage_page:?}, trying VID/PID only"
        );
        let info = api
            .device_list()
            .find(|info| info.vendor_id() == vid && info.product_id() == pid)
            .ok_or_else(|| anyhow::anyhow!("hidraw device {vid:04x}:{pid:04x} not found"))?;
        return info
            .open_device(&api)
            .map_err(|e| anyhow::anyhow!("hidraw open {vid:04x}:{pid:04x}: {e}"));
    }

    if candidates.len() > 1 && !port_numbers.is_empty() {
        if let Some(expected) = hidraw_path_for_usb_topology(bus, port_numbers) {
            if let Some(info) = candidates
                .iter()
                .find(|info| info.path() == expected.as_c_str())
            {
                debug!("Opening hidraw by topology match for {vid:04x}:{pid:04x}");
                return info
                    .open_device(&api)
                    .map_err(|e| anyhow::anyhow!("hidraw open by topology: {e}"));
            }
        }
    }

    debug!(
        "Opening first hidraw candidate for {vid:04x}:{pid:04x} ({} match(es))",
        candidates.len()
    );
    candidates[0]
        .open_device(&api)
        .map_err(|e| anyhow::anyhow!("hidraw open: {e}"))
}

pub fn open_shared_hid(
    device: &Device<GlobalContext>,
    usage_page: Option<u16>,
    vid: u16,
    pid: u16,
    bus: u8,
    port_numbers: &[u8],
    backend: HidBackend,
) -> Result<SharedHid> {
    match backend {
        HidBackend::Hidraw => {
            let dev = open_hidraw_device(vid, pid, bus, port_numbers, usage_page)?;
            Ok(Arc::new(Mutex::new(Box::new(HidrawTransport::new(dev)))))
        }
        HidBackend::Rusb => {
            let pn = port_numbers.to_vec();
            let transport = open_rusb_with_retry(device, || {
                let t = RusbHid::open_by_usage_with_reopener(
                    device.clone(),
                    usage_page,
                    make_rusb_reopener(vid, pid, bus, pn.clone(), usage_page),
                )?;
                Ok(t)
            })?;
            Ok(Arc::new(Mutex::new(Box::new(transport))))
        }
    }
}

pub fn open_hid_transient(
    device: &Device<GlobalContext>,
    usage_page: Option<u16>,
    vid: u16,
    pid: u16,
    bus: u8,
    port_numbers: &[u8],
    backend: HidBackend,
) -> Result<Box<dyn HidTransport>> {
    match backend {
        HidBackend::Hidraw => {
            let dev = open_hidraw_device(vid, pid, bus, port_numbers, usage_page)?;
            Ok(Box::new(HidrawTransport::new(dev)))
        }
        HidBackend::Rusb => {
            let transport = RusbHid::open_by_usage(device.clone(), usage_page)?;
            Ok(Box::new(transport))
        }
    }
}

pub fn open_hid_lcd_device(
    det: &DetectedDevice,
    backend: HidBackend,
) -> Option<Result<Box<dyn crate::traits::LcdDevice>>> {
    let pid = det.pid;
    let port_numbers = det.device.port_numbers().unwrap_or_default();

    let shared = match open_shared_hid(
        &det.device,
        det.hid_usage_page,
        det.vid,
        pid,
        det.bus,
        &port_numbers,
        backend,
    ) {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };

    match det.family {
        DeviceFamily::HydroShiftLcd | DeviceFamily::Galahad2Lcd => {
            shared.lock().read_flush();
            Some(
                crate::hydroshift_lcd::HydroShiftLcdController::new(shared, pid)
                    .map(|d| Box::new(d) as Box<dyn crate::traits::LcdDevice>),
            )
        }
        DeviceFamily::TlLcd => {
            let mut tl = crate::tl_lcd::TlLcdDevice::new(shared);
            Some(
                crate::traits::LcdDevice::initialize(&mut tl)
                    .map(|_| Box::new(tl) as Box<dyn crate::traits::LcdDevice>),
            )
        }
        _ => None,
    }
}

pub fn open_hid_lcd_by_topology(
    vid: u16,
    pid: u16,
    family: DeviceFamily,
    bus: u8,
    port_numbers: &[u8],
    backend: HidBackend,
) -> Result<Box<dyn crate::traits::LcdDevice>> {
    let device = rusb::devices()?
        .iter()
        .find(|d| {
            d.bus_number() == bus
                && d.port_numbers().ok().as_deref() == Some(port_numbers)
                && d.device_descriptor()
                    .map(|desc| desc.vendor_id() == vid && desc.product_id() == pid)
                    .unwrap_or(false)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no USB device matching topology {} for {vid:04x}:{pid:04x}",
                usb_topology_string(bus, port_numbers)
            )
        })?;
    let det = DetectedDevice {
        device: device.clone(),
        family,
        vid,
        pid,
        name: "",
        bus,
        address: device.address(),
        serial: None,
        hid_usage_page: None,
    };
    match open_hid_lcd_device(&det, backend) {
        Some(Ok(ctrl)) => Ok(ctrl),
        Some(Err(e)) => Err(e.context("HID LCD open by topology")),
        None => Err(anyhow::anyhow!("family does not support LCD")),
    }
}

pub fn open_hid_lcd_by_vid_pid(
    vid: u16,
    pid: u16,
    family: DeviceFamily,
    backend: HidBackend,
) -> Result<Box<dyn crate::traits::LcdDevice>> {
    let usb_device = find_usb_device(vid, pid);

    for attempt in 0..=3u32 {
        if let Some(device) = usb_device.as_ref() {
            let det = DetectedDevice {
                device: device.clone(),
                family,
                vid,
                pid,
                name: "",
                bus: device.bus_number(),
                address: device.address(),
                serial: None,
                hid_usage_page: None,
            };
            match open_hid_lcd_device(&det, backend) {
                Some(Ok(ctrl)) => return Ok(ctrl),
                Some(Err(e)) if attempt < 3 => {
                    warn!(
                        "HID LCD open attempt {} failed ({vid:04x}:{pid:04x}): {e}, resetting USB",
                        attempt + 1
                    );
                }
                Some(Err(e)) => {
                    return Err(e.context("HID LCD open failed after 4 attempts"));
                }
                None => return Err(anyhow::anyhow!("family does not support LCD")),
            }
        } else if attempt < 3 {
            warn!(
                "No USB device {vid:04x}:{pid:04x} found (attempt {}), retrying",
                attempt + 1
            );
        } else {
            return Err(anyhow::anyhow!(
                "no USB device found for {vid:04x}:{pid:04x} after 4 attempts"
            ));
        }

        if backend == HidBackend::Rusb {
            if let Some(ref usb_dev) = usb_device {
                let _ = RusbHid::reset_usb_device(usb_dev);
                std::thread::sleep(Duration::from_secs(3));
            }
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    unreachable!()
}

pub fn open_usb_bulk_backend(det: &DetectedDevice) -> Result<RusbBulk> {
    open_rusb_with_retry(&det.device, || {
        let mut transport = RusbBulk::open_device(det.device.clone())?;
        transport.detach_and_configure(det.name)?;
        Ok(transport)
    })
}
