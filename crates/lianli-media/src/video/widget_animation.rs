use crate::MediaError;
use image::codecs::{gif::GifDecoder, png::PngDecoder};
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, Frames, ImageDecoder, RgbaImage};
use lianli_shared::template::ImageFit;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) fn is_animation(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["gif", "png", "apng"]
                .iter()
                .any(|name| ext.eq_ignore_ascii_case(name))
        })
}

pub(super) fn decode(
    path: &Path,
    size: (u32, u32),
    fit: ImageFit,
    fps: f32,
    cancelled: &AtomicBool,
    mut emit: impl FnMut(RgbaImage, Duration) -> Result<(), MediaError>,
) -> Result<(), MediaError> {
    let file = BufReader::new(crate::image::open_image_file(path)?);
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gif"))
    {
        let mut decoder = GifDecoder::new(file)?;
        decoder.set_limits(crate::image::decode_limits())?;
        decode_frames(decoder.into_frames(), size, fit, fps, cancelled, emit)
    } else {
        let mut decoder = PngDecoder::with_limits(file, crate::image::decode_limits())?;
        if decoder.is_apng()? {
            decode_frames(
                decoder.apng()?.into_frames(),
                size,
                fit,
                fps,
                cancelled,
                emit,
            )
        } else {
            let mut limits = crate::image::decode_limits();
            limits.reserve(decoder.total_bytes())?;
            decoder.set_limits(limits)?;
            emit(
                fit_frame(DynamicImage::from_decoder(decoder)?, size, fit),
                Duration::from_millis(100),
            )
        }
    }
}

fn decode_frames(
    mut frames: Frames<'_>,
    size: (u32, u32),
    fit: ImageFit,
    fps: f32,
    cancelled: &AtomicBool,
    mut emit: impl FnMut(RgbaImage, Duration) -> Result<(), MediaError>,
) -> Result<(), MediaError> {
    let mut count = 0;
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let mut pending: Option<(RgbaImage, Duration)> = None;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(MediaError::Cancelled);
        }
        let Some(frame) = frames.next() else {
            break;
        };
        let frame = frame?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay = Duration::from_secs_f64(
            (f64::from(numer) / f64::from(denom.max(1)) / 1000.0).max(0.01),
        );
        if delay >= interval {
            if let Some((image, duration)) = pending.take() {
                emit(
                    fit_frame(DynamicImage::ImageRgba8(image), size, fit),
                    duration,
                )?;
            }
        }
        if let Some((image, duration)) = &mut pending {
            *image = frame.into_buffer();
            *duration += delay;
        } else {
            pending = Some((frame.into_buffer(), delay));
        }
        if pending
            .as_ref()
            .is_some_and(|(_, duration)| *duration >= interval)
        {
            let (image, duration) = pending.take().expect("pending animation frame");
            emit(
                fit_frame(DynamicImage::ImageRgba8(image), size, fit),
                duration,
            )?;
        }
        count += 1;
    }
    if let Some((image, duration)) = pending {
        emit(
            fit_frame(DynamicImage::ImageRgba8(image), size, fit),
            duration,
        )?;
    }
    if count == 0 {
        Err(MediaError::EmptyVideo)
    } else {
        Ok(())
    }
}

fn fit_frame(image: DynamicImage, size: (u32, u32), fit: ImageFit) -> RgbaImage {
    let (width, height) = size;
    if image.width() == width && image.height() == height {
        return image.into_rgba8();
    }
    match fit {
        ImageFit::Stretch => image
            .resize_exact(width, height, FilterType::Lanczos3)
            .into_rgba8(),
        ImageFit::Cover => image
            .resize_to_fill(width, height, FilterType::Lanczos3)
            .into_rgba8(),
        ImageFit::Contain => {
            let resized = image
                .resize(width, height, FilterType::Lanczos3)
                .into_rgba8();
            let mut canvas = RgbaImage::new(width, height);
            image::imageops::overlay(
                &mut canvas,
                &resized,
                i64::from((width - resized.width()) / 2),
                i64::from((height - resized.height()) / 2),
            );
            canvas
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rate_limit_skips_resize_work_but_preserves_duration_and_final_frame() {
        let frames = (0..100).map(|index| {
            Ok(image::Frame::from_parts(
                RgbaImage::from_pixel(2, 1, image::Rgba([index, 0, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(10, 1),
            ))
        });
        let mut output = Vec::new();
        decode_frames(
            Frames::new(Box::new(frames)),
            (4, 2),
            ImageFit::Stretch,
            5.0,
            &AtomicBool::new(false),
            |frame, delay| {
                output.push((frame, delay));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output.len(), 5);
        assert_eq!(
            output.iter().map(|(_, delay)| *delay).sum::<Duration>(),
            Duration::from_secs(1)
        );
        assert_eq!(output.last().unwrap().0.get_pixel(0, 0).0, [99, 0, 0, 255]);
        assert!(output
            .iter()
            .all(|(frame, delay)| frame.dimensions() == (4, 2)
                && *delay == Duration::from_millis(200)));
    }
}
