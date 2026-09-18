use crate::device_id::DeviceFamily;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StartupImageCapabilities {
    pub width: u32,
    pub height: u32,
    pub max_jpeg_bytes: usize,
    #[serde(default)]
    pub jpeg_target_bytes: Option<usize>,
}

pub fn capabilities(family: DeviceFamily) -> Option<StartupImageCapabilities> {
    use DeviceFamily::*;
    if family == WirelessAio {
        return Some(StartupImageCapabilities {
            width: 480,
            height: 480,
            max_jpeg_bytes: 1_048_576,
            jpeg_target_bytes: Some(20_480),
        });
    }
    if !matches!(family, Slv3Lcd | Tlv2Lcd | TlLcd | TlFlexLcd | SlInfFlexLcd) {
        return None;
    }
    let screen = crate::screen::screen_info_for(family)?;
    Some(StartupImageCapabilities {
        width: screen.width,
        height: screen.height,
        jpeg_target_bytes: None,
        max_jpeg_bytes: 101_888,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StartupImageState {
    Pending,
    Transferring,
    Transferred { response_received: bool },
    Failed { message: String },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupImageStatus {
    pub id: u64,
    pub device_id: String,
    pub status: StartupImageState,
}

impl StartupImageState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending | Self::Transferring)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_images_require_reachable_vendor_support() {
        use DeviceFamily::*;
        for family in [
            UniversalScreen,
            Lancool207,
            Vision9p2,
            HydroShift2Lcd,
            HydroShift2OledCurveLcd,
            UniversalScreenDesktop,
        ] {
            assert!(capabilities(family).is_none(), "{family:?}");
        }
        for family in [Slv3Lcd, Tlv2Lcd, TlLcd, TlFlexLcd, SlInfFlexLcd] {
            let caps = capabilities(family).unwrap();
            assert_eq!(caps.max_jpeg_bytes, 101_888);
            assert_eq!(caps.jpeg_target_bytes, None);
        }
        let tlv2 = capabilities(Tlv2Lcd).unwrap();
        assert_eq!((tlv2.width, tlv2.height), (400, 400));
    }

    #[test]
    fn older_capability_payloads_remain_readable() {
        let old: StartupImageCapabilities =
            serde_json::from_str(r#"{"width":480,"height":1920,"max_jpeg_bytes":1048576}"#)
                .unwrap();
        assert_eq!(old.jpeg_target_bytes, None);
        for family in [DeviceFamily::TlFlexLcd, DeviceFamily::SlInfFlexLcd] {
            let flex = capabilities(family).unwrap();
            assert_eq!(
                (flex.width, flex.height, flex.max_jpeg_bytes),
                (400, 400, 101_888)
            );
        }
    }

    #[test]
    fn h2_images_use_the_wireless_receiver_budget_only() {
        let caps = capabilities(DeviceFamily::WirelessAio).unwrap();
        assert_eq!((caps.width, caps.height), (480, 480));
        assert_eq!(caps.jpeg_target_bytes, Some(20_480));
        assert!(capabilities(DeviceFamily::HydroShift2Lcd).is_none());
        assert!(capabilities(DeviceFamily::HydroShift2LcdDesktop).is_none());
        assert!(capabilities(DeviceFamily::HydroShift2OledCurveLcd).is_none());
    }
}
