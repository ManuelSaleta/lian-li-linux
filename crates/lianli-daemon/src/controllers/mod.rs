//! Background controller threads for fans, AIO coolers, and RGB.
//!
//! - [`aio`] — AIO pump/fan controller (reads coolant temp, applies pump RPM).
//! - [`fan`] — Fan curve controller (reads sensors, applies PWM).
//! - [`rgb`] — RGB effect engine + direct-color writer + wireless RGB streaming.

pub mod aio;
pub(crate) mod cooling;
pub mod fan;
pub mod rgb;
