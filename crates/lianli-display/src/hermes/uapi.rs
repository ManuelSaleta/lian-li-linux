// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
// Layouts follow Hermes-KMS 7fbdd011cc74a57c52e1beea0f98c10c5fd5f198, UAPI 13.

pub const UAPI_VERSION: u32 = 13;
pub const DRIVER_NAME: &[u8] = b"hermes-kms";
pub const REQUIRED_CAPS: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 11)
    | (1 << 14)
    | (1 << 15)
    | (1 << 35);

#[repr(C)]
#[derive(Default)]
pub struct Version {
    pub uapi: u32,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub driver_name: [u8; 32],
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct Caps {
    pub flags: u64,
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub preferred_width: u32,
    pub preferred_height: u32,
    pub max_refresh_hz: u32,
    pub output_count: u32,
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct Status {
    pub flags: u64,
    pub frame_sequence: u64,
    pub last_update_ns: u64,
    pub last_enable_ns: u64,
    pub last_disable_ns: u64,
    pub connector_id: u32,
    pub crtc_id: u32,
    pub plane_id: u32,
    pub encoder_id: u32,
    pub requested_width: u32,
    pub requested_height: u32,
    pub requested_refresh_hz: u32,
    pub active_width: u32,
    pub active_height: u32,
    pub active_refresh_hz: u32,
    pub framebuffer_id: u32,
    pub framebuffer_width: u32,
    pub framebuffer_height: u32,
    pub framebuffer_format: u32,
    pub framebuffer_plane_count: u32,
    pub framebuffer_pitch: [u32; 4],
    pub framebuffer_offset: [u32; 4],
    pub reserved_alignment: u32,
    pub framebuffer_modifier: u64,
    pub session_id: u64,
    pub owner_pid: i32,
    pub reserved0: u32,
    pub bound_fd_count: u64,
    pub reserved: [u64; 5],
}

#[repr(C)]
#[derive(Default)]
pub struct Identity {
    pub driver_name: [u8; 32],
    pub output_name: [u8; 32],
    pub connector_name: [u8; 32],
    pub connector_id: u32,
    pub crtc_id: u32,
    pub plane_id: u32,
    pub encoder_id: u32,
    pub output_index: u32,
    pub output_count: u32,
    pub device_index: u32,
    pub device_count: u32,
    pub device_role: u32,
    pub session_index: u32,
    pub session_device_count: u32,
    pub cursor_plane_id: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct SelectOutput {
    pub output_index: u32,
    pub flags: u32,
    pub selected_output_index: u32,
    pub output_count: u32,
    pub output_name: [u8; 32],
    pub reserved: [u32; 8],
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct SetOutput {
    pub enabled: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub flags: u32,
    pub result_flags: u32,
    pub session_id: u64,
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct AcquireFrame {
    pub flags: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub modifier: u64,
    pub framebuffer_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub plane_count: u32,
    pub pitch: [u32; 4],
    pub offset: [u32; 4],
    pub dma_buf_fd: [i32; 4],
    pub sync_file_fd: i32,
    pub reserved0: u32,
    pub damage: [u32; 4],
    pub reserved: [u64; 6],
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct WaitUpdate {
    pub flags: u64,
    pub after_frame_sequence: u64,
    pub after_cursor_sequence: u64,
    pub frame_sequence: u64,
    pub cursor_sequence: u64,
    pub frame_timestamp_ns: u64,
    pub cursor_timestamp_ns: u64,
    pub status_flags: u64,
    pub timeout_ms: u32,
    pub reserved0: u32,
    pub reserved: [u64; 5],
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct AcquireCursor {
    pub flags: u64,
    pub sequence: u64,
    pub image_sequence: u64,
    pub timestamp_ns: u64,
    pub modifier: u64,
    pub session_id: u64,
    pub position_x: i32,
    pub position_y: i32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub framebuffer_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub plane_count: u32,
    pub pitch: [u32; 4],
    pub offset: [u32; 4],
    pub dma_buf_fd: [i32; 4],
    pub sync_file_fd: i32,
    pub reserved0: u32,
    pub reserved_alignment: u32,
    pub reserved: [u64; 6],
}

#[repr(C, align(8))]
#[derive(Default)]
pub struct SessionAccess {
    pub token: [u64; 2],
    pub session_id: u64,
    pub operation: u32,
    pub output_index: u32,
    pub flags: u32,
    pub result_flags: u32,
    pub reserved: [u64; 4],
}

pub trait Request: sealed::Sealed {
    const IOCTL: libc::c_ulong;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! requests {
    ($($type:ty: $command:expr, $direction:expr);+ $(;)?) => {$(
        impl sealed::Sealed for $type {}
        impl Request for $type {
            const IOCTL: libc::c_ulong = (($direction << 30) | ((std::mem::size_of::<Self>() as u32) << 16) | ((b'd' as u32) << 8) | (0x40 + $command)) as libc::c_ulong;
        }
    )+};
}

requests! {
    Version: 0x00, 2;
    Caps: 0x01, 2;
    Status: 0x02, 2;
    SetOutput: 0x03, 3;
    AcquireFrame: 0x04, 3;
    Identity: 0x05, 2;
    SelectOutput: 0x08, 3;
    SessionAccess: 0x09, 3;
    AcquireCursor: 0x0a, 3;
    WaitUpdate: 0x0b, 3;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn matches_the_pinned_upstream_c_abi() {
        assert_eq!(size_of::<Version>(), 48);
        assert_eq!(Version::IOCTL, 0x80306440);
        assert_eq!(size_of::<Caps>(), 40);
        assert_eq!(Caps::IOCTL, 0x80286441);
        assert_eq!(size_of::<Status>(), 208);
        assert_eq!(Status::IOCTL, 0x80d06442);
        assert_eq!(offset_of!(Status, framebuffer_modifier), 136);
        assert_eq!(offset_of!(Status, bound_fd_count), 160);
        assert_eq!(size_of::<SetOutput>(), 32);
        assert_eq!(SetOutput::IOCTL, 0xc0206443);
        assert_eq!(size_of::<AcquireFrame>(), 176);
        assert_eq!(AcquireFrame::IOCTL, 0xc0b06444);
        assert_eq!(offset_of!(AcquireFrame, dma_buf_fd), 84);
        assert_eq!(offset_of!(AcquireFrame, reserved), 128);
        assert_eq!(size_of::<Identity>(), 144);
        assert_eq!(Identity::IOCTL, 0x80906445);
        assert_eq!(size_of::<SelectOutput>(), 80);
        assert_eq!(SelectOutput::IOCTL, 0xc0506448);
        assert_eq!(size_of::<SessionAccess>(), 72);
        assert_eq!(SessionAccess::IOCTL, 0xc0486449);
        assert_eq!(size_of::<AcquireCursor>(), 224);
        assert_eq!(AcquireCursor::IOCTL, 0xc0e0644a);
        assert_eq!(offset_of!(AcquireCursor, dma_buf_fd), 148);
        assert_eq!(offset_of!(AcquireCursor, reserved), 176);
        assert_eq!(size_of::<WaitUpdate>(), 112);
        assert_eq!(WaitUpdate::IOCTL, 0xc070644b);
    }
}
