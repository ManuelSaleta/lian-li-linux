//! Transport layer for the Lian Li Linux daemon and device drivers.
//!
//! Two concrete transports, both implemented on `rusb` (libusb):
//!
//! - [`RusbHid`] — HID-over-USB: feature reports, input reports, output
//!   reports. Used by every wired HID device (ENE 6K77, TL Fan, TL LCD,
//!   Galahad2, HydroShift LCD). Includes optional self-healing via a
//!   [`RusbHidReopener`] closure that re-acquires the device on I/O failure.
//!
//! - [`RusbBulk`] — WinUSB-style bulk transfers. Used by RF dongles, WinUSB
//!   LCDs (HydroShift II / Lancool 207 / Universal Screen), LED controllers,
//!   and TURZX desktop-mode displays.

pub mod error;
pub mod hid;
pub mod hid_trait;
pub mod hidraw;
pub mod usb;

pub use error::TransportError;
pub use hid::{RusbHid, RusbHidReopener};
pub use hid_trait::HidTransport;
pub use hidraw::HidrawTransport;
pub use usb::RusbBulk;
