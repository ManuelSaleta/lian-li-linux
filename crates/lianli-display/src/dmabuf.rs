use crate::frame::{Mode, PixelFormat};
use crate::MAX_FRAME_BYTES;
use anyhow::{ensure, Context, Result};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct Plane {
    pub descriptor: OwnedFd,
    pub pitch: u32,
    pub offset: u32,
    pub allocation_bytes: u64,
}

pub struct DmaImage {
    pub buffer: DmaBuffer,
    pub fence: OwnedFd,
}

pub struct DmaBuffer {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<Plane>,
}

impl std::ops::Deref for DmaImage {
    type Target = DmaBuffer;
    fn deref(&self) -> &DmaBuffer {
        &self.buffer
    }
}

impl std::ops::DerefMut for DmaImage {
    fn deref_mut(&mut self) -> &mut DmaBuffer {
        &mut self.buffer
    }
}

impl DmaBuffer {
    pub fn validate(&self) -> Result<()> {
        Mode {
            width: self.width,
            height: self.height,
            refresh_hz: 1,
        }
        .validate()?;
        ensure!(
            PixelFormat::from_fourcc(self.fourcc)? == self.format,
            "Inconsistent DMA-BUF pixel format"
        );
        ensure!(
            !self.planes.is_empty() && self.planes.len() <= 4,
            "Invalid DMA-BUF plane count"
        );
        for plane in &self.planes {
            ensure!(
                plane.allocation_bytes > 0 && plane.allocation_bytes <= 4 * MAX_FRAME_BYTES as u64,
                "DMA-BUF allocation exceeds 256 MiB"
            );
            ensure!(
                plane.pitch > 0 && u64::from(plane.offset) < plane.allocation_bytes,
                "Invalid DMA-BUF plane layout"
            );
        }
        if self.modifier == 0 {
            ensure!(
                self.planes.len() == 1,
                "Packed linear RGB requires one plane"
            );
            let plane = &self.planes[0];
            let row = u64::from(self.width) * 4;
            let end =
                u64::from(plane.offset) + u64::from(plane.pitch) * u64::from(self.height - 1) + row;
            ensure!(
                u64::from(plane.pitch) >= row && end <= plane.allocation_bytes,
                "DMA-BUF is smaller than its pixel layout"
            );
        }
        Ok(())
    }
}

impl DmaImage {
    pub fn wait_ready(&self, timeout: Duration, cancel: &AtomicBool) -> Result<()> {
        wait_fence(self.fence.as_fd(), timeout, cancel)
    }
}

pub fn allocation_size(descriptor: BorrowedFd<'_>) -> Result<u64> {
    let size = unsafe { libc::lseek(descriptor.as_raw_fd(), 0, libc::SEEK_END) };
    ensure!(size > 0, "DMA-BUF did not report its allocation size");
    Ok(size as u64)
}

fn wait_fence(fence: BorrowedFd<'_>, timeout: Duration, cancel: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "DMA-BUF synchronization cancelled"
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "DMA-BUF synchronization timed out");
        let mut fd = libc::pollfd {
            fd: fence.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().clamp(1, 50) as i32) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Waiting for DMA-BUF producer");
        }
        if result > 0 {
            ensure!(
                fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0,
                "DMA-BUF synchronization fence failed"
            );
            if fd.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut info = FenceInfo::default();
            let result = unsafe {
                libc::ioctl(
                    fence.as_raw_fd(),
                    0xc0383e04 as libc::c_ulong,
                    &mut info as *mut FenceInfo,
                )
            };
            ensure!(
                result == 0 && info.status > 0,
                "DMA-BUF producer failed or supplied an invalid synchronization fence"
            );
            return Ok(());
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct FenceInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    count: u32,
    pad: u32,
    details: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_plane_extent_before_import_and_rejects_non_fence_descriptors() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(64).unwrap();
        let fence = tempfile::tempfile().unwrap();
        let mut image = DmaImage {
            buffer: DmaBuffer {
                width: 2,
                height: 3,
                format: PixelFormat::Xrgb8888,
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
                planes: vec![Plane {
                    descriptor: file.into(),
                    pitch: 16,
                    offset: 8,
                    allocation_bytes: 64,
                }],
            },
            fence: fence.into(),
        };
        image.validate().unwrap();
        image.planes[0].allocation_bytes = 47;
        assert!(image.validate().is_err());
        image.planes[0].allocation_bytes = 64;
        image.planes[0].pitch = 4;
        assert!(image.validate().is_err());
        assert!(image
            .wait_ready(Duration::from_millis(100), &AtomicBool::new(false))
            .is_err());
        assert_eq!(std::mem::size_of::<FenceInfo>(), 56);
    }
}
