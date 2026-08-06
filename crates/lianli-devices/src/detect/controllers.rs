use crate::registry::SharedHid;
use anyhow::Result;
use lianli_shared::device_id::DeviceFamily;
use std::sync::Arc;

pub fn create_hid_lcd_device(
    family: DeviceFamily,
    pid: u16,
    backend: SharedHid,
) -> Option<Result<Box<dyn crate::traits::LcdDevice>>> {
    match family {
        DeviceFamily::HydroShiftLcd | DeviceFamily::Galahad2Lcd => Some(
            crate::hydroshift_lcd::HydroShiftLcdController::new(backend, pid)
                .map(|d| Box::new(Arc::new(d)) as Box<dyn crate::traits::LcdDevice>),
        ),
        DeviceFamily::TlLcd => {
            let mut tl = crate::tl_lcd::TlLcdDevice::new(backend);
            Some(
                crate::traits::LcdDevice::initialize(&mut tl)
                    .map(|_| Box::new(tl) as Box<dyn crate::traits::LcdDevice>),
            )
        }
        _ => None,
    }
}
