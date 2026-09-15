use anyhow::{ensure, Context, Result};
use lianli_display::frame::{Mode, PixelFormat};
use lianli_display::Capture;
use lianli_media::video::H264Encoder;
use lianli_shared::display::{
    CpuReadbackReason, DesktopEncoder, DesktopEncodingStatus, DisplayCodec, SoftwareEncodingReason,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub struct Encoder {
    cpu_input: Option<H264Encoder>,
    cpu_readback_reason: Option<CpuReadbackReason>,
    software_reason: Option<SoftwareEncodingReason>,
    status: Option<DesktopEncodingStatus>,
}

#[derive(Clone, Copy)]
pub struct FrameRequest {
    pub codec: DisplayCodec,
    pub mode: Mode,
    pub format: PixelFormat,
    pub fps: u32,
    pub hardware_video: bool,
}

impl Encoder {
    pub fn status(&self) -> Option<DesktopEncodingStatus> {
        self.status
    }

    pub fn reset(&mut self, _capture: &mut dyn Capture) -> Result<()> {
        self.cpu_input = None;
        self.cpu_readback_reason = None;
        self.software_reason = None;
        self.status = None;
        Ok(())
    }

    pub fn encode(
        &mut self,
        capture: &mut dyn Capture,
        request: FrameRequest,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        self.cpu_readback_reason = request
            .hardware_video
            .then_some(CpuReadbackReason::NoDmaBuf);
        let frame = capture.frame(cancel)?;
        ensure!(
            frame.mode == request.mode && frame.format == request.format,
            "Desktop frame layout changed unexpectedly"
        );
        let result = match request.codec {
            DisplayCodec::Jpeg => Ok(turbojpeg::compress(
                turbojpeg::Image {
                    pixels: frame.pixels,
                    width: frame.mode.width as usize,
                    height: frame.mode.height as usize,
                    pitch: frame.stride,
                    format: if frame.format.rgb_byte_order() {
                        turbojpeg::PixelFormat::RGBX
                    } else {
                        turbojpeg::PixelFormat::BGRX
                    },
                },
                90,
                turbojpeg::Subsamp::Sub2x2,
            )?
            .to_vec()),
            DisplayCodec::H264 => {
                if self.cpu_input.is_none() {
                    self.cpu_input = Some(H264Encoder::new(
                        request.mode.width,
                        request.mode.height,
                        request.fps,
                        request.format.rgb_byte_order(),
                        request.hardware_video,
                    )?);
                }
                let result = complete_cpu_packet(
                    self.cpu_input.as_mut().unwrap(),
                    frame.pixels,
                    frame.stride,
                );
                match result {
                    Ok(packet) => Ok(packet),
                    Err(error) if self.cpu_input.as_ref().unwrap().backend() != "libx264" => {
                        tracing::warn!(
                            "Desktop hardware encoding failed; switching to libx264: {error:#}"
                        );
                        self.cpu_input = None;
                        self.software_reason = Some(SoftwareEncodingReason::HardwareFailed);
                        ensure!(
                            !cancel.load(Ordering::Relaxed),
                            "Desktop encoding cancelled"
                        );
                        self.cpu_input = Some(H264Encoder::new(
                            request.mode.width,
                            request.mode.height,
                            request.fps,
                            request.format.rgb_byte_order(),
                            false,
                        )?);
                        complete_cpu_packet(
                            self.cpu_input.as_mut().unwrap(),
                            frame.pixels,
                            frame.stride,
                        )
                    }
                    Err(error) => Err(error),
                }
            }
        };
        if result.is_ok() {
            let encoder = if request.codec == DisplayCodec::Jpeg {
                DesktopEncoder::Turbojpeg
            } else {
                match self
                    .cpu_input
                    .as_ref()
                    .context("Missing desktop encoder")?
                    .backend()
                {
                    "libx264" => DesktopEncoder::Libx264,
                    "h264_nvenc" => DesktopEncoder::H264Nvenc,
                    "h264_amf" => DesktopEncoder::H264Amf,
                    _ => DesktopEncoder::Unknown,
                }
            };
            if encoder == DesktopEncoder::Libx264
                && request.hardware_video
                && self.software_reason.is_none()
            {
                self.software_reason = Some(SoftwareEncodingReason::HardwareUnavailable);
            }
            self.status = Some(DesktopEncodingStatus {
                encoder,
                gpu_input: false,
                cpu_readback_reason: self.cpu_readback_reason,
                software_reason: self.software_reason,
            });
        }
        result
    }
}

fn complete_cpu_packet(encoder: &mut H264Encoder, pixels: &[u8], stride: usize) -> Result<Vec<u8>> {
    let packet = encoder.encode_strided(pixels, stride)?;
    ensure!(
        !packet.is_empty(),
        "Desktop encoding requires a complete packet for a static frame"
    );
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_display::{frame::Frame, Event};
    use std::time::Duration;

    struct SoftwareCapture {
        pixels: Vec<u8>,
        frames: usize,
    }

    impl Capture for SoftwareCapture {
        fn poll_events(&mut self, _: Duration, _: &AtomicBool) -> Result<Vec<Event>> {
            Ok(Vec::new())
        }
        fn request_update(&mut self) -> Result<bool> {
            Ok(true)
        }
        fn frame(&mut self, _: &AtomicBool) -> Result<Frame<'_>> {
            self.frames += 1;
            Frame::new(mode(), PixelFormat::Argb8888, 64, &self.pixels)
        }
        fn invalidate(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn mode() -> Mode {
        Mode {
            width: 16,
            height: 16,
            refresh_hz: 30,
        }
    }

    #[test]
    fn cpu_packets_report_the_selected_encoder_and_reset_status() {
        for (codec, hardware_video) in [
            (DisplayCodec::H264, false),
            (DisplayCodec::Jpeg, false),
            (DisplayCodec::Jpeg, true),
        ] {
            let mut capture = SoftwareCapture {
                pixels: vec![80; 16 * 16 * 4],
                frames: 0,
            };
            let mut encoder = Encoder::default();
            let packet = encoder
                .encode(
                    &mut capture,
                    FrameRequest {
                        codec,
                        mode: mode(),
                        format: PixelFormat::Argb8888,
                        fps: 30,
                        hardware_video,
                    },
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert!(!packet.is_empty());
            assert_eq!(capture.frames, 1);
            let status = encoder.status().unwrap();
            assert_eq!(
                status.encoder,
                if codec == DisplayCodec::Jpeg {
                    DesktopEncoder::Turbojpeg
                } else {
                    DesktopEncoder::Libx264
                }
            );
            assert!(!status.gpu_input);
            assert!(status.software_reason.is_none());
            encoder.reset(&mut capture).unwrap();
            assert!(encoder.status().is_none());
        }
    }
}
