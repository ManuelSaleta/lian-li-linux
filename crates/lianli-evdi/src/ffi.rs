//! ABI declarations checked against libevdi 1.14 and 1.15.

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_uint, c_void};

pub enum evdi_device_context {}
pub type evdi_handle = *mut evdi_device_context;
pub type evdi_selectable = c_int;

pub const AVAILABLE: c_int = 0;
pub const UNRECOGNIZED: c_int = 1;
pub const NOT_PRESENT: c_int = 2;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct evdi_rect {
    pub x1: c_int,
    pub y1: c_int,
    pub x2: c_int,
    pub y2: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct evdi_mode {
    pub width: c_int,
    pub height: c_int,
    pub refresh_rate: c_int,
    pub bits_per_pixel: c_int,
    pub pixel_format: c_uint,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct evdi_buffer {
    pub id: c_int,
    pub buffer: *mut c_void,
    pub width: c_int,
    pub height: c_int,
    pub stride: c_int,
    pub rects: *mut evdi_rect,
    pub rect_count: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct evdi_cursor_set {
    pub hot_x: i32,
    pub hot_y: i32,
    pub width: u32,
    pub height: u32,
    pub enabled: u8,
    pub buffer_length: u32,
    pub buffer: *mut u32,
    pub pixel_format: u32,
    pub stride: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct evdi_cursor_move {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct evdi_ddcci_data {
    pub address: u16,
    pub flags: u16,
    pub buffer_length: u32,
    pub buffer: *mut u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct evdi_event_context {
    pub dpms_handler: Option<extern "C" fn(dpms_mode: c_int, user_data: *mut c_void)>,
    pub mode_changed_handler: Option<extern "C" fn(mode: evdi_mode, user_data: *mut c_void)>,
    pub update_ready_handler:
        Option<extern "C" fn(buffer_to_be_updated: c_int, user_data: *mut c_void)>,
    pub crtc_state_handler: Option<extern "C" fn(state: c_int, user_data: *mut c_void)>,
    pub cursor_set_handler:
        Option<extern "C" fn(cursor_set: evdi_cursor_set, user_data: *mut c_void)>,
    pub cursor_move_handler:
        Option<extern "C" fn(cursor_move: evdi_cursor_move, user_data: *mut c_void)>,
    pub ddcci_data_handler:
        Option<extern "C" fn(ddcci_data: evdi_ddcci_data, user_data: *mut c_void)>,
    pub user_data: *mut c_void,
}

#[repr(C)]
pub struct evdi_lib_version {
    pub version_major: c_int,
    pub version_minor: c_int,
    pub version_patchlevel: c_int,
}

pub struct Api {
    pub check_device: unsafe extern "C" fn(c_int) -> c_int,
    pub open: unsafe extern "C" fn(c_int) -> evdi_handle,
    pub add_device: unsafe extern "C" fn() -> c_int,
    pub close: unsafe extern "C" fn(evdi_handle),
    pub connect2: unsafe extern "C" fn(evdi_handle, *const u8, c_uint, u32, u32),
    pub disconnect: unsafe extern "C" fn(evdi_handle),
    pub register_buffer: unsafe extern "C" fn(evdi_handle, evdi_buffer),
    pub unregister_buffer: unsafe extern "C" fn(evdi_handle, c_int),
    pub request_update: unsafe extern "C" fn(evdi_handle, c_int) -> bool,
    pub grab_pixels: unsafe extern "C" fn(evdi_handle, *mut evdi_rect, *mut c_int),
    pub handle_events: unsafe extern "C" fn(evdi_handle, *mut evdi_event_context),
    pub get_event_ready: unsafe extern "C" fn(evdi_handle) -> evdi_selectable,
    pub ddcci_response: unsafe extern "C" fn(evdi_handle, *const u8, u32, bool),
    pub version: (i32, i32, i32),
    _library: libloading::Library,
}

impl Api {
    pub fn load() -> anyhow::Result<std::sync::Arc<Self>> {
        use anyhow::Context;
        // SONAME first; some source installations only provide the unversioned name.
        let library = unsafe { libloading::Library::new("libevdi.so.1") }
            .or_else(|_| unsafe { libloading::Library::new("libevdi.so") })
            .context(
                "EVDI userspace library is unavailable; install libevdi for EVDI desktop mode",
            )?;
        Self::from_library(library).map(std::sync::Arc::new)
    }

    fn from_library(library: libloading::Library) -> anyhow::Result<Self> {
        use anyhow::Context;
        // Function signatures match the checked ABI, and the library outlives every pointer.
        unsafe {
            let get_version: libloading::Symbol<unsafe extern "C" fn(*mut evdi_lib_version)> =
                library
                    .get(b"evdi_get_lib_version\0")
                    .context("libevdi lacks its version query")?;
            let mut version = evdi_lib_version {
                version_major: 0,
                version_minor: 0,
                version_patchlevel: 0,
            };
            get_version(&mut version);
            let version = (
                version.version_major,
                version.version_minor,
                version.version_patchlevel,
            );
            validate_version(version)?;
            Ok(Self {
                check_device: *library.get(b"evdi_check_device\0")?,
                open: *library.get(b"evdi_open\0")?,
                add_device: *library.get(b"evdi_add_device\0")?,
                close: *library.get(b"evdi_close\0")?,
                connect2: *library.get(b"evdi_connect2\0")?,
                disconnect: *library.get(b"evdi_disconnect\0")?,
                register_buffer: *library.get(b"evdi_register_buffer\0")?,
                unregister_buffer: *library.get(b"evdi_unregister_buffer\0")?,
                request_update: *library.get(b"evdi_request_update\0")?,
                grab_pixels: *library.get(b"evdi_grab_pixels\0")?,
                handle_events: *library.get(b"evdi_handle_events\0")?,
                get_event_ready: *library.get(b"evdi_get_event_ready\0")?,
                ddcci_response: *library.get(b"evdi_ddcci_response\0")?,
                version,
                _library: library,
            })
        }
    }
}

fn validate_version(version: (i32, i32, i32)) -> anyhow::Result<()> {
    if version.0 != 1 || !(14..=15).contains(&version.1) || version.2 < 0 {
        anyhow::bail!(
            "Unsupported libevdi {}.{}.{}; EVDI requires the checked 1.14 or 1.15 ABI",
            version.0,
            version.1,
            version.2
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unchecked_library_abis() {
        for version in [(0, 0, 0), (1, 13, 9), (1, 16, 0), (2, 14, 0), (1, 14, -1)] {
            assert!(validate_version(version).is_err());
        }
        assert!(validate_version((1, 14, 8)).is_ok());
        assert!(validate_version((1, 15, 0)).is_ok());
    }

    #[test]
    fn library_without_evdi_symbols_fails_before_device_access() {
        let library = unsafe { libloading::Library::new("libc.so.6") }.unwrap();
        let error = Api::from_library(library).err().unwrap();
        assert!(error.to_string().contains("version query"));
    }
}
