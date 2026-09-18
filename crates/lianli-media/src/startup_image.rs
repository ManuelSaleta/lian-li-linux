use anyhow::{ensure, Context, Result};
use image::{DynamicImage, ImageDecoder};
use lianli_shared::startup_image::StartupImageCapabilities;
use std::io::Cursor;

pub fn prepare(jpeg: &[u8], caps: StartupImageCapabilities) -> Result<Vec<u8>> {
    ensure!(
        !jpeg.is_empty() && jpeg.len() <= caps.max_jpeg_bytes,
        "Startup JPEG exceeds the device payload limit"
    );
    let mut decoder =
        image::codecs::jpeg::JpegDecoder::new(Cursor::new(jpeg)).context("Invalid startup JPEG")?;
    ensure!(
        decoder.dimensions() == (caps.width, caps.height),
        "Startup image must be {}×{} pixels",
        caps.width,
        caps.height
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(caps.width);
    limits.max_image_height = Some(caps.height);
    limits.max_alloc = Some(32 * 1024 * 1024);
    decoder.set_limits(limits)?;
    let image = DynamicImage::from_decoder(decoder)?.into_rgb8();
    if let Some(target_bytes) = caps.jpeg_target_bytes {
        let limit = target_bytes.min(caps.max_jpeg_bytes);
        let source = turbojpeg::Image {
            pixels: image.as_raw().as_slice(),
            width: caps.width as usize,
            pitch: caps.width as usize * 3,
            height: caps.height as usize,
            format: turbojpeg::PixelFormat::RGB,
        };
        for quality in (5..=95).rev().step_by(5) {
            let prepared = turbojpeg::compress(source, quality, turbojpeg::Subsamp::Sub2x2)
                .context("Encoding startup JPEG")?;
            if prepared.len() <= limit {
                return Ok(prepared.to_vec());
            }
        }
        anyhow::bail!(
            "Startup image cannot fit the {limit}-byte JPEG budget; choose a simpler crop"
        );
    }
    let mut prepared = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut prepared, 95).encode_image(&image)?;
    ensure!(
        prepared.len() <= caps.max_jpeg_bytes,
        "Prepared startup JPEG is {} bytes; the device limit is {}. Choose a simpler image or crop",
        prepared.len(),
        caps.max_jpeg_bytes
    );
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vendor_jpeg_budget_reduces_quality_and_uses_live_image_chroma() {
        let mut seed = 12345u32;
        let source = image::RgbImage::from_fn(64, 64, |_, _| {
            let mut color = [0; 3];
            for byte in &mut color {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *byte = seed as u8;
            }
            image::Rgb(color)
        });
        let mut input = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut input, 95)
            .encode_image(&source)
            .unwrap();
        let caps = StartupImageCapabilities {
            width: 64,
            height: 64,
            max_jpeg_bytes: 65_536,
            jpeg_target_bytes: Some(2048),
        };
        assert!(input.len() > 2048);
        let prepared = prepare(&input, caps).unwrap();
        assert!(prepared.len() <= 2048);
        let header = turbojpeg::read_header(&prepared).unwrap();
        assert_eq!((header.width, header.height), (64, 64));
        assert_eq!(header.subsamp, turbojpeg::Subsamp::Sub2x2);
        assert!(prepare(
            &input,
            StartupImageCapabilities {
                jpeg_target_bytes: Some(1),
                ..caps
            }
        )
        .is_err());
    }

    #[test]
    fn startup_media_requires_the_native_size_and_valid_bounded_jpeg() {
        let caps = StartupImageCapabilities {
            width: 4,
            height: 3,
            max_jpeg_bytes: 4096,
            jpeg_target_bytes: None,
        };
        assert!(prepare(b"not a jpeg", caps).is_err());
        let image = image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]));
        let mut input = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut input)
            .encode_image(&image)
            .unwrap();
        let output = prepare(&input, caps).unwrap();
        assert_eq!(image::load_from_memory(&output).unwrap().width(), 4);
        assert!(prepare(&input, StartupImageCapabilities { width: 3, ..caps }).is_err());
        assert!(prepare(
            &input,
            StartupImageCapabilities {
                max_jpeg_bytes: 1,
                ..caps
            }
        )
        .is_err());
    }
}
