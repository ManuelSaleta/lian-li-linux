use crate::evdi::EvdiCapture;
use crate::hyprland::Control;
use crate::wayland::HyprlandCapture;
use crate::{Capture, OutputRequest};
use anyhow::{ensure, Result};
use std::sync::atomic::{AtomicBool, Ordering};

pub enum LocalBackend {
    Hyprland(Control),
    Drm,
}

pub struct OpenedCapture {
    pub capture: Box<dyn Capture>,
    pub backend: &'static str,
    pub fallback_reason: Option<String>,
}

impl LocalBackend {
    pub fn discover() -> Result<Self> {
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if requires_native(signature.is_some(), &desktop) {
            Ok(Self::Hyprland(Control::from_env()?))
        } else {
            Ok(Self::Drm)
        }
    }

    pub fn open(self, request: OutputRequest, cancel: &AtomicBool) -> Result<OpenedCapture> {
        match self {
            Self::Hyprland(control) => Ok(OpenedCapture {
                capture: Box::new(HyprlandCapture::open(control, request, cancel)?),
                backend: "Hyprland native",
                fallback_reason: None,
            }),
            Self::Drm => {
                ensure!(
                    !cancel.load(Ordering::Relaxed),
                    "Desktop capture startup cancelled"
                );
                Ok(OpenedCapture {
                    capture: Box::new(EvdiCapture::open(request)?),
                    backend: "EVDI",
                    fallback_reason: None,
                })
            }
        }
    }
}

fn requires_native(signature_present: bool, desktop: &str) -> bool {
    signature_present
        || desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("Hyprland"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprland_session_requires_native_capture_even_when_its_socket_is_missing() {
        assert!(requires_native(true, ""));
        assert!(requires_native(false, "Hyprland"));
        assert!(requires_native(false, "uwsm:hyprland"));
        assert!(!requires_native(false, "KDE"));
        assert!(!requires_native(false, ""));
    }
}
