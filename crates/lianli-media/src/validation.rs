use anyhow::{ensure, Context, Result};
use image::ImageReader;
use lianli_shared::media_dependencies::AssetKind;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::process::Command;

pub fn check_still(mut file: File, kind: AssetKind, extension: Option<&str>) -> Result<()> {
    ensure!(file.metadata()?.is_file(), "Media must be a regular file");
    file.rewind()?;
    match kind {
        AssetKind::File => {
            let mut byte = [0];
            let _read = file.read(&mut byte)?;
        }
        AssetKind::Image | AssetKind::Gif => {
            let reader = if kind == AssetKind::Gif {
                ImageReader::with_format(BufReader::new(file), image::ImageFormat::Gif)
            } else {
                let format = extension
                    .and_then(image::ImageFormat::from_extension)
                    .context("Image filename needs a supported extension matching its format.")?;
                ImageReader::with_format(BufReader::new(file), format)
            };
            let decoded = crate::image::decode_image(reader).context("Decoding image")?;
            ensure!(
                decoded.width() > 0 && decoded.height() > 0,
                "Image has no pixels"
            );
        }
        AssetKind::Font => {
            crate::fonts::decode(file)?;
        }
        AssetKind::Video => anyhow::bail!("Video validation requires the isolated FFmpeg process"),
    }
    Ok(())
}

/// Execute this command in place of the isolated validation helper. Its stdin
/// must remain the opened regular media file, with process limits already set.
pub fn video_command() -> Command {
    let mut command = Command::new("/usr/bin/ffmpeg");
    command.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-xerror",
        "-abort_on",
        "empty_output",
        "-max_alloc",
        "67108864",
        "-cpucount",
        "1",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
        "-threads",
        "1",
        "-hwaccel",
        "none",
        "-max_pixels",
        "16777216",
        "-probesize",
        "4194304",
        "-analyzeduration",
        "3000000",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
        "/proc/self/fd/0",
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-frames:v",
        "1",
        "-threads",
        "1",
        "-f",
        "null",
        "-",
    ]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn still_validation_uses_the_descriptor_and_the_renderers_filename_format() {
        let mut file = tempfile::tempfile().unwrap();
        image::RgbImage::from_pixel(2, 3, image::Rgb([40, 50, 60]))
            .write_to(&mut file, image::ImageFormat::Png)
            .unwrap();
        check_still(file.try_clone().unwrap(), AssetKind::Image, Some("png")).unwrap();
        assert!(check_still(file.try_clone().unwrap(), AssetKind::Image, Some("bmp")).is_err());
        assert!(check_still(file.try_clone().unwrap(), AssetKind::Image, None).is_err());
        assert!(check_still(file, AssetKind::Font, None).is_err());
        let mut invalid = tempfile::tempfile().unwrap();
        invalid.write_all(b"readable but not an image").unwrap();
        assert!(check_still(invalid, AssetKind::Image, Some("png")).is_err());
    }

    #[test]
    fn oversized_fonts_and_image_dimensions_are_rejected() {
        let font = tempfile::tempfile().unwrap();
        font.set_len(32 * 1024 * 1024 + 1).unwrap();
        assert!(check_still(font, AssetKind::Font, None)
            .unwrap_err()
            .to_string()
            .contains("32 MiB"));
        let mut image = tempfile::tempfile().unwrap();
        image::RgbImage::new(8193, 1)
            .write_to(&mut image, image::ImageFormat::Png)
            .unwrap();
        assert!(check_still(image, AssetKind::Image, Some("png")).is_err());
    }
}
