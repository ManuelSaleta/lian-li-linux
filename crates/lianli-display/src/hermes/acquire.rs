use super::{call, uapi, OwnedOutput};
use crate::dmabuf::{allocation_size, DmaImage, Plane};
use crate::frame::PixelFormat;
use anyhow::{ensure, Context, Result};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct AcquiredFrame {
    pub image: DmaImage,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub damage: Option<[u32; 4]>,
}

pub struct AcquiredCursor {
    pub image: Option<DmaImage>,
    pub sequence: u64,
    pub image_sequence: u64,
    pub timestamp_ns: u64,
    pub visible: bool,
    pub destination: [i32; 4],
    pub source: [u32; 4],
}

impl OwnedOutput {
    pub fn wait_update(
        &self,
        frame: u64,
        cursor: u64,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Option<uapi::WaitUpdate>> {
        ensure!(!cancel.load(Ordering::Relaxed), "Hermes capture cancelled");
        let mut request = uapi::WaitUpdate {
            after_frame_sequence: frame,
            after_cursor_sequence: cursor,
            timeout_ms: timeout.as_millis().min(100) as u32,
            ..Default::default()
        };
        match call(&self.node.file, &mut request) {
            Ok(()) => {
                ensure!(request.flags & 3 != 0, "Hermes wait returned no update");
                Ok(Some(request))
            }
            Err(error) if transient(&error, &[libc::ETIMEDOUT, libc::EAGAIN, libc::EINTR]) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn acquire_frame(&self) -> Result<Option<AcquiredFrame>> {
        let mut request = uapi::AcquireFrame {
            flags: (1 << 0) | (1 << 5),
            dma_buf_fd: [-1; 4],
            sync_file_fd: -1,
            ..Default::default()
        };
        match call(&self.node.file, &mut request) {
            Ok(()) => {}
            Err(error)
                if transient(
                    &error,
                    &[libc::ESTALE, libc::ENODATA, libc::EAGAIN, libc::EINTR],
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        }
        let descriptors = unsafe { Descriptors::adopt(request.dma_buf_fd, request.sync_file_fd) }?;
        ensure!(
            request.flags & 14 == 14,
            "Hermes returned incomplete frame metadata or synchronization"
        );
        if request.flags & (1 << 6) != 0 {
            let [x1, y1, x2, y2] = request.damage;
            ensure!(
                x1 <= x2 && y1 <= y2 && x2 <= request.width && y2 <= request.height,
                "Invalid Hermes damage rectangle"
            );
        }
        let image = descriptors.image(
            (request.width, request.height),
            request.format,
            request.modifier,
            request.plane_count,
            request.pitch,
            request.offset,
        )?;
        Ok(Some(AcquiredFrame {
            image,
            sequence: request.sequence,
            timestamp_ns: request.timestamp_ns,
            damage: (request.flags & (1 << 6) != 0).then_some(request.damage),
        }))
    }

    pub fn acquire_cursor(&self) -> Result<Option<AcquiredCursor>> {
        let mut request = uapi::AcquireCursor {
            flags: 3,
            dma_buf_fd: [-1; 4],
            sync_file_fd: -1,
            ..Default::default()
        };
        match call(&self.node.file, &mut request) {
            Ok(()) => {}
            Err(error) if transient(&error, &[libc::ESTALE, libc::EAGAIN, libc::EINTR]) => {
                return Ok(None)
            }
            Err(error) => return Err(error),
        }
        let descriptors = unsafe { Descriptors::adopt(request.dma_buf_fd, request.sync_file_fd) }?;
        ensure!(
            request.flags & (1 << 2) != 0 && request.session_id == self.session_id,
            "Hermes cursor belongs to another session"
        );
        let visible = request.flags & (1 << 3) != 0;
        let buffered = request.flags & (1 << 6) != 0;
        let image = if buffered {
            ensure!(
                request.flags & ((1 << 7) | (1 << 8)) == (1 << 7) | (1 << 8),
                "Hermes cursor has no synchronized buffer"
            );
            ensure!(
                request.width <= 1024 && request.height <= 1024,
                "Hermes cursor exceeds 1024 pixels"
            );
            Some(descriptors.image(
                (request.width, request.height),
                request.format,
                request.modifier,
                request.plane_count,
                request.pitch,
                request.offset,
            )?)
        } else {
            ensure!(
                descriptors.planes.iter().all(Option::is_none) && descriptors.fence.is_none(),
                "Unexpected hidden-cursor descriptors"
            );
            None
        };
        if visible {
            ensure!(
                request.flags & (1 << 9) != 0 && buffered,
                "Visible Hermes cursor has no geometry or pixels"
            );
            ensure!(
                request.crtc_w > 0
                    && request.crtc_h > 0
                    && request.crtc_w <= 4096
                    && request.crtc_h <= 4096,
                "Invalid cursor destination size"
            );
            ensure!(
                request.src_w > 0
                    && request.src_h > 0
                    && u64::from(request.src_x) + u64::from(request.src_w)
                        <= u64::from(request.width) << 16
                    && u64::from(request.src_y) + u64::from(request.src_h)
                        <= u64::from(request.height) << 16,
                "Cursor source rectangle exceeds its buffer"
            );
        }
        Ok(Some(AcquiredCursor {
            image,
            sequence: request.sequence,
            image_sequence: request.image_sequence,
            timestamp_ns: request.timestamp_ns,
            visible,
            destination: [
                request.crtc_x,
                request.crtc_y,
                request.crtc_w as i32,
                request.crtc_h as i32,
            ],
            source: [request.src_x, request.src_y, request.src_w, request.src_h],
        }))
    }
}

fn transient(error: &anyhow::Error, codes: &[i32]) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .is_some_and(|code| codes.contains(&code))
}

struct Descriptors {
    planes: [Option<OwnedFd>; 4],
    fence: Option<OwnedFd>,
}

impl Descriptors {
    unsafe fn adopt(raw_planes: [i32; 4], raw_fence: i32) -> Result<Self> {
        let mut seen = Vec::with_capacity(5);
        let mut duplicate = false;
        let mut adopt = |fd| {
            if fd < 0 {
                return None;
            }
            if seen.contains(&fd) {
                duplicate = true;
                return None;
            }
            seen.push(fd);
            // Successful driver ioctls return newly owned, close-on-exec descriptors.
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        };
        let descriptors = Self {
            planes: raw_planes.map(&mut adopt),
            fence: adopt(raw_fence),
        };
        ensure!(!duplicate, "Hermes returned duplicate owned descriptors");
        Ok(descriptors)
    }

    fn image(
        mut self,
        dimensions: (u32, u32),
        fourcc: u32,
        modifier: u64,
        count: u32,
        pitch: [u32; 4],
        offset: [u32; 4],
    ) -> Result<DmaImage> {
        let (width, height) = dimensions;
        ensure!(
            (1..=4).contains(&count),
            "Invalid Hermes buffer plane count"
        );
        let mut planes = Vec::with_capacity(count as usize);
        for index in 0..4 {
            if index < count as usize {
                let descriptor = self.planes[index]
                    .take()
                    .context("Missing Hermes plane descriptor")?;
                let allocation_bytes = allocation_size(descriptor.as_fd())?;
                planes.push(Plane {
                    descriptor,
                    pitch: pitch[index],
                    offset: offset[index],
                    allocation_bytes,
                });
            } else {
                ensure!(
                    self.planes[index].is_none(),
                    "Unexpected extra Hermes plane descriptor"
                );
            }
        }
        let image = DmaImage {
            buffer: crate::dmabuf::DmaBuffer {
                width,
                height,
                format: PixelFormat::from_fourcc(fourcc)?,
                fourcc,
                modifier,
                planes,
            },
            fence: self
                .fence
                .take()
                .context("Missing Hermes synchronization fence")?,
        };
        image.validate()?;
        Ok(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn duplicate_descriptors_are_closed_once_even_when_a_response_is_rejected() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        let fd = sender.into_raw_fd();
        assert!(unsafe { Descriptors::adopt([fd, fd, -1, -1], -1) }.is_err());
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(receiver.read(&mut [0]).unwrap(), 0);
    }
}
