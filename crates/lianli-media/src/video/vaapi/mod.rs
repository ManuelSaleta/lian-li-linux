mod drm;

use anyhow::{ensure, Context, Result};
pub use drm::{DmaFrame, DmaPlane};
use ffmpeg_next::{self as ffmpeg, ffi};
use std::os::fd::{BorrowedFd, IntoRawFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub struct VaapiEncoder {
    encoder: ffmpeg::encoder::Video,
    conversion: ffmpeg::filter::Graph,
    import_frames: HardwareBuffer,
    width: u32,
    height: u32,
    fourcc: u32,
    start: Instant,
    failed: bool,
}

impl VaapiEncoder {
    pub fn new(device: BorrowedFd<'_>, frame: &DmaFrame<'_>, fps: u32) -> Result<Self> {
        super::ensure_ffmpeg_initialized();
        frame.validate()?;
        ensure!(frame.modifier == 0, "VAAPI requires a linear RGB buffer. Legacy PRIME import cannot preserve tiling modifiers.");
        ensure!((1..=120).contains(&fps), "Invalid VAAPI frame rate");
        let codec = ffmpeg::encoder::find_by_name("h264_vaapi")
            .context("FFmpeg has no VAAPI H.264 encoder")?;
        let device = create_device(device)?;
        let import_frames = HardwareBuffer::new(unsafe { ffi::av_hwframe_ctx_alloc(device.0) })?;
        unsafe {
            let context = (*import_frames.0).data.cast::<ffi::AVHWFramesContext>();
            (*context).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*context).sw_format = frame.pixel_format()?;
            (*context).width = frame.width as i32;
            (*context).height = frame.height as i32;
            (*context).initial_pool_size = 0;
            check(
                ffi::av_hwframe_ctx_init(import_frames.0),
                "Initializing VAAPI RGB import",
            )?;
        }
        let mut conversion = conversion_graph(&import_frames, frame, fps)?;
        let output = conversion
            .get("out")
            .context("VAAPI output filter is missing")?;
        let output_frames = unsafe { ffi::av_buffersink_get_hw_frames_ctx(output.as_ptr()) };
        ensure!(
            !output_frames.is_null(),
            "VAAPI conversion produced no hardware frames"
        );
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        encoder.set_width(frame.width);
        encoder.set_height(frame.height);
        encoder.set_format(ffmpeg::format::Pixel::VAAPI);
        encoder.set_time_base((1, 1_000_000));
        encoder.set_frame_rate(Some((fps as i32, 1)));
        super::h264_inprocess::desktop_rate_control(&mut encoder, frame.width, frame.height, fps);
        encoder.set_gop((fps / 2).max(1));
        encoder.set_max_b_frames(0);
        unsafe {
            let context = encoder.as_mut_ptr();
            (*context).hw_frames_ctx = reference(output_frames)?;
            (*context).thread_count = 1;
            (*context).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*context).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*context).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*context).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        }
        let mut options = ffmpeg::Dictionary::new();
        options.set("rc_mode", "VBR");
        options.set("async_depth", "1");
        options.set("profile", "constrained_baseline");
        let encoder = encoder
            .open_with(options)
            .context("Opening DMA-BUF VAAPI H.264 encoder")?;
        Ok(Self {
            encoder,
            conversion,
            import_frames,
            width: frame.width,
            height: frame.height,
            fourcc: frame.fourcc,
            start: Instant::now(),
            failed: false,
        })
    }

    /// The caller must finish GPU writes before submission and reuse the allocation only after success.
    /// Recreate the encoder after any failure; pending GPU work must not receive another frame.
    pub fn encode(&mut self, input: &DmaFrame<'_>, cancel: &AtomicBool) -> Result<Vec<u8>> {
        ensure!(
            !self.failed,
            "Recreate the VAAPI encoder after a failed submission"
        );
        self.failed = true;
        let result = self.encode_frame(input, cancel);
        if result.is_ok() {
            self.failed = false;
        }
        result
    }

    fn encode_frame(&mut self, input: &DmaFrame<'_>, cancel: &AtomicBool) -> Result<Vec<u8>> {
        ensure!(!cancel.load(Ordering::Relaxed), "VAAPI encoding cancelled");
        ensure!(
            (input.width, input.height, input.fourcc) == (self.width, self.height, self.fourcc),
            "DMA-BUF format changed. Recreate the VAAPI encoder."
        );
        ensure!(
            input.modifier == 0,
            "VAAPI input must retain its linear layout"
        );
        let mut source = input.import()?;
        source.set_pts(Some(
            self.start.elapsed().as_micros().min(i64::MAX as u128) as i64
        ));
        let mut mapped = ffmpeg::frame::Video::empty();
        unsafe {
            (*mapped.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            (*mapped.as_mut_ptr()).hw_frames_ctx = reference(self.import_frames.0)?;
            check(
                ffi::av_hwframe_map(
                    mapped.as_mut_ptr(),
                    source.as_ptr(),
                    ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
                ),
                "Importing DMA-BUF into VAAPI without CPU mapping",
            )?;
            check(
                ffi::av_frame_copy_props(mapped.as_mut_ptr(), source.as_ptr()),
                "Copying capture color metadata",
            )?;
        }
        ensure!(!cancel.load(Ordering::Relaxed), "VAAPI encoding cancelled");
        let mut input_filter = self
            .conversion
            .get("in")
            .context("VAAPI input filter is missing")?;
        unsafe {
            check(
                ffi::av_buffersrc_add_frame(input_filter.as_mut_ptr(), mapped.as_mut_ptr()),
                "Submitting GPU color conversion",
            )?;
        }
        let mut converted = ffmpeg::frame::Video::empty();
        self.conversion
            .get("out")
            .context("VAAPI output filter is missing")?
            .sink()
            .frame(&mut converted)
            .context("Receiving GPU NV12 conversion")?;
        ensure!(
            converted.format() == ffmpeg::format::Pixel::VAAPI
                && converted.width() == self.width
                && converted.height() == self.height,
            "GPU conversion returned an unexpected frame layout"
        );
        ensure!(!cancel.load(Ordering::Relaxed), "VAAPI encoding cancelled");
        self.encoder
            .send_frame(&converted)
            .context("Submitting VAAPI H.264 frame")?;
        let mut packet = ffmpeg::Packet::empty();
        let mut bytes = Vec::new();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        ensure!(
                            bytes.len().saturating_add(data.len()) <= 8 * 1024 * 1024,
                            "VAAPI frame exceeds the device payload budget"
                        );
                        bytes.extend_from_slice(data);
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(error) => return Err(error).context("Receiving VAAPI H.264 frame"),
            }
        }
        ensure!(
            !bytes.is_empty(),
            "VAAPI output is delayed. Capture requires a complete packet."
        );
        ensure!(!cancel.load(Ordering::Relaxed), "VAAPI encoding cancelled");
        Ok(bytes)
    }
}

fn conversion_graph(
    frames: &HardwareBuffer,
    frame: &DmaFrame<'_>,
    fps: u32,
) -> Result<ffmpeg::filter::Graph> {
    let mut graph = ffmpeg::filter::Graph::new();
    let source = ffmpeg::filter::find("buffer").context("FFmpeg buffer filter is missing")?;
    unsafe {
        (*graph.as_mut_ptr()).nb_threads = 1;
        let input =
            ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(), source.as_ptr(), c"in".as_ptr());
        ensure!(!input.is_null(), "Allocating VAAPI input filter failed");
        let parameters = ffi::av_buffersrc_parameters_alloc();
        ensure!(
            !parameters.is_null(),
            "Allocating VAAPI filter parameters failed"
        );
        (*parameters).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*parameters).time_base = ffi::AVRational {
            num: 1,
            den: 1_000_000,
        };
        (*parameters).frame_rate = ffi::AVRational {
            num: fps as i32,
            den: 1,
        };
        (*parameters).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
        (*parameters).width = frame.width as i32;
        (*parameters).height = frame.height as i32;
        (*parameters).hw_frames_ctx = frames.0;
        let result = ffi::av_buffersrc_parameters_set(input, parameters);
        ffi::av_free(parameters.cast());
        check(result, "Configuring VAAPI input filter")?;
        check(
            ffi::avfilter_init_str(input, ptr::null()),
            "Initializing VAAPI input filter",
        )?;
    }
    graph.add(
        &ffmpeg::filter::find("buffersink").context("FFmpeg buffersink filter is missing")?,
        "out",
        "",
    )?;
    graph.output("in", 0)?.input("out", 0)?.parse("scale_vaapi=format=nv12:out_color_matrix=bt709:out_range=limited:out_color_primaries=bt709:out_color_transfer=bt709")?;
    graph
        .validate()
        .context("Configuring GPU RGB-to-NV12 conversion")?;
    Ok(graph)
}

fn create_device(descriptor: BorrowedFd<'_>) -> Result<HardwareBuffer> {
    let descriptor = descriptor.try_clone_to_owned()?;
    let drm = HardwareBuffer::new(unsafe {
        ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM)
    })?;
    unsafe {
        let context = (*drm.0).data.cast::<ffi::AVHWDeviceContext>();
        let device = (*context).hwctx.cast::<ffi::AVDRMDeviceContext>();
        (*device).fd = descriptor.into_raw_fd();
        check(
            ffi::av_hwdevice_ctx_init(drm.0),
            "Initializing capture render device for VAAPI",
        )?;
        let mut vaapi = ptr::null_mut();
        let result = ffi::av_hwdevice_ctx_create_derived(
            &mut vaapi,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            drm.0,
            0,
        );
        let owned = HardwareBuffer(vaapi);
        check(result, "Creating VAAPI device on the capture GPU")?;
        ensure!(!owned.0.is_null(), "VAAPI returned no device");
        Ok(owned)
    }
}

struct HardwareBuffer(*mut ffi::AVBufferRef);

impl HardwareBuffer {
    fn new(pointer: *mut ffi::AVBufferRef) -> Result<Self> {
        ensure!(
            !pointer.is_null(),
            "Allocating FFmpeg hardware context failed"
        );
        Ok(Self(pointer))
    }
}

impl Drop for HardwareBuffer {
    fn drop(&mut self) {
        unsafe {
            ffi::av_buffer_unref(&mut self.0);
        }
    }
}

fn reference(buffer: *mut ffi::AVBufferRef) -> Result<*mut ffi::AVBufferRef> {
    let pointer = unsafe { ffi::av_buffer_ref(buffer) };
    ensure!(
        !pointer.is_null(),
        "Retaining FFmpeg hardware context failed"
    );
    Ok(pointer)
}

fn check(status: i32, operation: &str) -> Result<()> {
    if status < 0 {
        return Err(ffmpeg::Error::from(status)).with_context(|| operation.to_owned());
    }
    Ok(())
}
