use lianli_media::video::encode_h264;
use lianli_shared::screen::ScreenInfo;

fn test_screen() -> ScreenInfo {
    ScreenInfo {
        width: 16,
        height: 16,
        max_fps: 30,
        jpeg_quality: 90,
        max_payload: 512_000,
        h264: false,
        needs_keepalive: false,
        png: false,
        play_count: 0,
    }
}

#[test]
fn encode_h264_succeeds_with_gif_input() {
    let temp = tempfile::TempDir::new().unwrap();
    let gif_path = temp.path().join("test.gif");

    let file = std::fs::File::create(&gif_path).unwrap();
    let mut encoder = image::codecs::gif::GifEncoder::new(file);
    let img1 = image::RgbImage::from_pixel(16, 16, image::Rgb([255, 0, 0]));
    let img2 = image::RgbImage::from_pixel(16, 16, image::Rgb([0, 255, 0]));
    encoder
        .encode(&img1, 16, 16, image::ExtendedColorType::Rgb8)
        .unwrap();
    encoder
        .encode(&img2, 16, 16, image::ExtendedColorType::Rgb8)
        .unwrap();
    drop(encoder);

    let result = encode_h264(&gif_path, 10.0, 0.0, &test_screen());
    assert!(
        result.is_ok(),
        "encode_h264 must succeed with GIF input, got: {:?}",
        result.err()
    );
}

#[test]
fn encode_h264_succeeds_with_jpeg_input() {
    let temp = tempfile::TempDir::new().unwrap();
    let jpg_path = temp.path().join("color.jpg");

    let img = image::RgbImage::from_pixel(16, 16, image::Rgb([0, 0, 255]));
    img.save(&jpg_path).unwrap();

    let result = encode_h264(&jpg_path, 10.0, 0.0, &test_screen());
    assert!(
        result.is_ok(),
        "encode_h264 must succeed with JPEG input, got: {:?}",
        result.err()
    );
}

#[test]
fn encode_h264_jpeg_output_has_multiple_frames() {
    let temp = tempfile::TempDir::new().unwrap();
    let jpg_path = temp.path().join("color.jpg");
    let img = image::RgbImage::from_pixel(16, 16, image::Rgb([0, 255, 0]));
    img.save(&jpg_path).unwrap();

    let (h264_path, _temp_dir, _fps) = encode_h264(&jpg_path, 30.0, 0.0, &test_screen()).unwrap();

    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&h264_path)
        .output()
        .expect("ffprobe not found");
    let frame_count: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(
        frame_count > 1,
        "single-image encode must produce multi-frame H264, got {frame_count}"
    );
}
