use anyhow::{ensure, Context, Result};
use ffmpeg_next::{ffi, frame::Video};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

pub struct DmaPlane<'a> {
    pub descriptor: BorrowedFd<'a>,
    pub allocation_bytes: u64,
    pub pitch: u32,
    pub offset: u32,
}

pub struct DmaFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: &'a [DmaPlane<'a>],
}

impl DmaFrame<'_> {
    pub(super) fn pixel_format(&self) -> Result<ffi::AVPixelFormat> {
        use ffi::AVPixelFormat::*;
        match self.fourcc.to_le_bytes() {
            [b'A', b'R', b'2', b'4'] => Ok(AV_PIX_FMT_BGRA),
            [b'X', b'R', b'2', b'4'] => Ok(AV_PIX_FMT_BGR0),
            [b'A', b'B', b'2', b'4'] => Ok(AV_PIX_FMT_RGBA),
            [b'X', b'B', b'2', b'4'] => Ok(AV_PIX_FMT_RGB0),
            _ => anyhow::bail!("Unsupported DMA-BUF RGB format"),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.width > 0
                && self.height > 0
                && self.width.is_multiple_of(2)
                && self.height.is_multiple_of(2),
            "H.264 DMA-BUF dimensions must be positive and even"
        );
        ensure!(
            u64::from(self.width)
                .checked_mul(u64::from(self.height))
                .and_then(|pixels| pixels.checked_mul(4))
                .is_some_and(|bytes| bytes <= 64 * 1024 * 1024),
            "DMA-BUF frame exceeds 64 MiB"
        );
        self.pixel_format()?;
        ensure!(
            (1..=4).contains(&self.planes.len()),
            "DMA-BUF must have one to four planes"
        );
        for plane in self.planes {
            ensure!(
                (1..=256 * 1024 * 1024).contains(&plane.allocation_bytes)
                    && plane.pitch > 0
                    && u64::from(plane.offset) < plane.allocation_bytes,
                "Invalid DMA-BUF plane allocation or layout"
            );
        }
        if self.modifier == 0 {
            ensure!(
                self.planes.len() == 1,
                "Linear packed RGB must use one plane"
            );
            let plane = &self.planes[0];
            let row_bytes = u64::from(self.width) * 4;
            let end = u64::from(plane.offset)
                + u64::from(self.height - 1) * u64::from(plane.pitch)
                + row_bytes;
            ensure!(
                u64::from(plane.pitch) >= row_bytes && end <= plane.allocation_bytes,
                "DMA-BUF rows exceed the exported allocation"
            );
        }
        Ok(())
    }

    pub(super) fn import(&self) -> Result<Video> {
        self.validate()?;
        let mut owned = Box::new(Imported {
            descriptor: unsafe { std::mem::zeroed() },
            planes: Vec::with_capacity(self.planes.len()),
        });
        owned.descriptor.nb_objects = self.planes.len() as i32;
        owned.descriptor.nb_layers = 1;
        owned.descriptor.layers[0].format = self.fourcc;
        owned.descriptor.layers[0].nb_planes = self.planes.len() as i32;
        for (index, plane) in self.planes.iter().enumerate() {
            let fd = plane.descriptor.try_clone_to_owned()?;
            owned.descriptor.objects[index] = ffi::AVDRMObjectDescriptor {
                fd: fd.as_raw_fd(),
                size: plane
                    .allocation_bytes
                    .try_into()
                    .context("DMA-BUF allocation exceeds address space")?,
                format_modifier: self.modifier,
            };
            owned.descriptor.layers[0].planes[index] = ffi::AVDRMPlaneDescriptor {
                object_index: index as i32,
                offset: plane
                    .offset
                    .try_into()
                    .context("DMA-BUF offset exceeds address space")?,
                pitch: plane
                    .pitch
                    .try_into()
                    .context("DMA-BUF stride exceeds address space")?,
            };
            owned.planes.push(fd);
        }
        let mut frame = Video::empty();
        let data = (&mut owned.descriptor as *mut ffi::AVDRMFrameDescriptor).cast();
        let opaque = Box::into_raw(owned);
        // FFmpeg retains the descriptors until the last reference to this imported frame is gone.
        unsafe {
            let buffer = ffi::av_buffer_create(
                data,
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(release),
                opaque.cast(),
                ffi::AV_BUFFER_FLAG_READONLY,
            );
            if buffer.is_null() {
                drop(Box::from_raw(opaque));
                anyhow::bail!("Allocating DMA-BUF frame reference failed");
            }
            let raw = frame.as_mut_ptr();
            (*raw).buf[0] = buffer;
            (*raw).data[0] = data;
            (*raw).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*raw).width = self.width as i32;
            (*raw).height = self.height as i32;
            (*raw).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
            (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
        }
        Ok(frame)
    }
}

struct Imported {
    descriptor: ffi::AVDRMFrameDescriptor,
    planes: Vec<OwnedFd>,
}

unsafe extern "C" fn release(opaque: *mut c_void, _: *mut u8) {
    unsafe {
        drop(Box::from_raw(opaque.cast::<Imported>()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn imported_descriptors_live_through_the_last_ffmpeg_reference() {
        let (source, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let planes = [DmaPlane {
            descriptor: source.as_fd(),
            allocation_bytes: 40,
            pitch: 24,
            offset: 0,
        }];
        let input = DmaFrame {
            width: 4,
            height: 2,
            fourcc: u32::from_le_bytes(*b"AR24"),
            modifier: 0,
            planes: &planes,
        };
        let frame = input.import().unwrap();
        let mut retained = Video::empty();
        assert_eq!(
            unsafe { ffi::av_frame_ref(retained.as_mut_ptr(), frame.as_ptr()) },
            0
        );
        let layout = unsafe { &*(*retained.as_ptr()).data[0].cast::<ffi::AVDRMFrameDescriptor>() };
        assert_eq!(layout.layers[0].planes[0].pitch, 24);
        assert_eq!(layout.objects[0].size, 40);
        drop(frame);
        drop(source);
        assert_eq!(
            peer.read(&mut [0]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(retained);
        let mut event = libc::pollfd {
            fd: peer.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut event, 1, 3000) }, 1);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn invalid_layout_is_rejected_before_gpu_import() {
        let file = tempfile::tempfile().unwrap();
        let mut planes = [DmaPlane {
            descriptor: file.as_fd(),
            allocation_bytes: 39,
            pitch: 24,
            offset: 0,
        }];
        let mut input = DmaFrame {
            width: 4,
            height: 2,
            fourcc: u32::from_le_bytes(*b"AR24"),
            modifier: 0,
            planes: &planes,
        };
        assert!(input.import().is_err());
        input.width = 3;
        assert!(input.import().is_err());
        input.width = u32::MAX - 1;
        input.height = u32::MAX - 1;
        assert!(input.import().is_err());
        planes[0].allocation_bytes = 40;
        let input = DmaFrame {
            width: 4,
            height: 2,
            fourcc: u32::from_le_bytes(*b"NV12"),
            modifier: 0,
            planes: &planes,
        };
        assert!(input.import().is_err());
    }
}
