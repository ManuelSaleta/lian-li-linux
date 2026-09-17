use super::*;
use std::fs;
use std::process::Command;

fn color_clip(root: &Path) -> std::path::PathBuf {
    repeated_color_clip(root, 1)
}

fn repeated_color_clip(root: &Path, repeats: usize) -> std::path::PathBuf {
    let raw = root.join("frames.rgba");
    let pixels: Vec<u8> = [50, 100, 150, 200]
        .into_iter()
        .cycle()
        .take(4 * repeats)
        .flat_map(|red| std::iter::repeat_n([red, 0, 0, 255], 16 * 8).flatten())
        .collect();
    fs::write(&raw, pixels).unwrap();
    let path = root.join("colors.mkv");
    let result = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            "16x8",
            "-framerate",
            "4",
            "-i",
        ])
        .arg(raw)
        .args(["-threads", "1", "-c:v", "ffv1", "-pix_fmt", "bgra"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    path
}

fn wait_for_frame(stream: &VideoStream) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pending = stream.shared.0.lock();
    while pending.ready.is_empty() && !pending.finished {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "decoder did not produce a frame");
        stream.shared.1.wait_for(&mut pending, remaining);
    }
}

#[test]
fn small_video_loops_from_cache_without_a_decoder() {
    let root = tempfile::tempdir().unwrap();
    let path = color_clip(root.path());
    let mut stream = VideoStream::new(
        &path,
        4.0,
        (16, 8),
        ImageFit::Stretch,
        true,
        &PreparationControl::new(false),
    )
    .unwrap();
    assert!(stream.worker.is_none());
    assert!(stream.shared.0.lock().finished);
    fs::remove_file(path).unwrap();
    let start = Instant::now();
    stream.advance(start).unwrap();
    for index in 1..20 {
        assert!(stream
            .advance(start + Duration::from_millis(250 * index))
            .unwrap());
        assert_eq!(
            stream.frame().get_pixel(0, 0).0,
            [
                [50, 0, 0, 255],
                [100, 0, 0, 255],
                [150, 0, 0, 255],
                [200, 0, 0, 255]
            ][index as usize % 4]
        );
    }
}

fn animation_fixture(path: &Path, frames: usize) {
    let file = fs::File::create(path).unwrap();
    if path.extension().unwrap() == "gif" {
        let mut encoder = gif::Encoder::new(file, 2, 1, &[0, 0, 0, 255, 0, 0, 0, 0, 255]).unwrap();
        for index in 0..frames {
            let frame = gif::Frame {
                width: if index == 0 { 2 } else { 1 },
                height: 1,
                left: if index == 0 { 0 } else { 1 },
                delay: if index % 2 == 0 { 10 } else { 30 },
                dispose: if index == 1 {
                    gif::DisposalMethod::Previous
                } else {
                    gif::DisposalMethod::Keep
                },
                transparent: Some(0),
                buffer: if index == 0 {
                    vec![1, 0].into()
                } else {
                    vec![if index % 2 == 1 { 2 } else { 0 }].into()
                },
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).unwrap();
        }
    } else {
        let mut encoder = png::Encoder::new(file, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(frames as u32, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        for index in 0..frames {
            writer
                .set_frame_delay(if index % 2 == 0 { 1 } else { 3 }, 10)
                .unwrap();
            writer
                .set_dispose_op(if index == 1 {
                    png::DisposeOp::Previous
                } else {
                    png::DisposeOp::None
                })
                .unwrap();
            writer.set_blend_op(png::BlendOp::Over).unwrap();
            if index == 0 {
                writer
                    .write_image_data(&[255, 0, 0, 255, 0, 0, 0, 0])
                    .unwrap();
            } else {
                writer.set_frame_dimension(1, 1).unwrap();
                writer.set_frame_position(1, 0).unwrap();
                writer
                    .write_image_data(if index % 2 == 1 {
                        &[0, 0, 255, 255]
                    } else {
                        &[0, 0, 0, 0]
                    })
                    .unwrap();
            }
        }
        writer.finish().unwrap();
    }
}

#[test]
fn gif_and_apng_cache_preserves_delays_transparency_and_previous_disposal() {
    let root = tempfile::tempdir().unwrap();
    for extension in ["gif", "apng"] {
        let path = root.path().join(format!("animation.{extension}"));
        animation_fixture(&path, 3);
        let mut stream = VideoStream::new(
            &path,
            30.0,
            (2, 1),
            ImageFit::Stretch,
            true,
            &PreparationControl::new(false),
        )
        .unwrap();
        assert!(stream.worker.is_none());
        assert_eq!(stream.frame().get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(stream.frame().get_pixel(1, 0).0[3], 0);
        let start = Instant::now();
        stream.advance(start).unwrap();
        assert!(!stream.advance(start + Duration::from_millis(50)).unwrap());
        assert!(stream.advance(start + Duration::from_millis(100)).unwrap());
        assert_eq!(stream.frame().get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert!(!stream.advance(start + Duration::from_millis(350)).unwrap());
        assert!(stream.advance(start + Duration::from_millis(400)).unwrap());
        assert_eq!(stream.frame().get_pixel(1, 0).0[3], 0);
        assert!(stream.advance(start + Duration::from_millis(500)).unwrap());
        assert_eq!(stream.frame().get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(stream.frame().get_pixel(1, 0).0[3], 0);
    }
}

#[test]
fn long_gif_and_apng_stream_past_the_old_frame_limit_and_loop() {
    let root = tempfile::tempdir().unwrap();
    for extension in ["gif", "apng"] {
        let path = root.path().join(format!("long.{extension}"));
        animation_fixture(&path, 8193);
        let mut stream = VideoStream::new(
            &path,
            30.0,
            (2, 1),
            ImageFit::Stretch,
            true,
            &PreparationControl::new(false),
        )
        .unwrap();
        assert!(stream.shared.0.lock().cache.is_none());
        let mut now = Instant::now();
        stream.advance(now).unwrap();
        for index in 1..=8194 {
            wait_for_frame(&stream);
            now += stream.current.as_ref().unwrap().duration;
            assert!(stream.advance(now).unwrap(), "{extension} frame {index}");
            let pending = stream.shared.0.lock();
            assert!(pending.ready.len() <= stream.queue_capacity);
            assert!(pending.reusable.is_empty());
        }
        assert_eq!(stream.frame().get_pixel(1, 0).0, [0, 0, 255, 255]);
        let shared = stream.shared.clone();
        drop(stream);
        assert!(shared.0.lock().finished);
    }
}

#[test]
fn queue_depth_balances_frame_rate_and_frame_memory() {
    assert_eq!(queue_capacity(1920 * 480 * 4, 30.0), 4);
    assert_eq!(queue_capacity(400 * 400 * 4, 60.0), 8);
    assert_eq!(queue_capacity(400 * 400 * 4, 1.0), 2);
    assert_eq!(queue_capacity(64 * 1024 * 1024, 60.0), 2);
}

#[test]
fn streamed_loop_preserves_frames_and_backpressure_and_stops_when_full() {
    let root = tempfile::tempdir().unwrap();
    let path = repeated_color_clip(root.path(), 65);
    let mut stream = VideoStream::new(
        &path,
        4.0,
        (16, 8),
        ImageFit::Stretch,
        true,
        &PreparationControl::new(false),
    )
    .unwrap();
    let start = Instant::now();
    assert_eq!(stream.frame().get_pixel(0, 0).0, [50, 0, 0, 255]);
    assert!(!stream.advance(start).unwrap());
    for index in 1..=524 {
        wait_for_frame(&stream);
        assert!(stream
            .advance(start + Duration::from_millis(250 * index))
            .unwrap());
        assert_eq!(
            stream.frame().get_pixel(0, 0).0,
            [
                [50, 0, 0, 255],
                [100, 0, 0, 255],
                [150, 0, 0, 255],
                [200, 0, 0, 255]
            ][index as usize % 4]
        );
        assert!(stream.shared.0.lock().ready.len() <= stream.queue_capacity);
    }
    let shared = stream.shared.clone();
    {
        let mut pending = shared.0.lock();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending.ready.len() < stream.queue_capacity {
            assert!(!pending.finished);
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero());
            shared.1.wait_for(&mut pending, remaining);
        }
    }
    let stopping = Instant::now();
    drop(stream);
    assert!(stopping.elapsed() < Duration::from_secs(2));
    assert!(shared.0.lock().finished);
}

#[test]
fn non_looping_stream_holds_the_last_frame() {
    let root = tempfile::tempdir().unwrap();
    let path = color_clip(root.path());
    let mut stream = VideoStream::new(
        &path,
        4.0,
        (16, 8),
        ImageFit::Stretch,
        false,
        &PreparationControl::new(false),
    )
    .unwrap();
    let start = Instant::now();
    stream.advance(start).unwrap();
    for index in 1..4 {
        wait_for_frame(&stream);
        assert!(stream
            .advance(start + Duration::from_millis(250 * index))
            .unwrap());
    }
    wait_for_frame(&stream);
    assert!(!stream.advance(start + Duration::from_secs(10)).unwrap());
    assert_eq!(stream.frame().get_pixel(0, 0).0, [200, 0, 0, 255]);
    assert!(stream.shared.0.lock().finished);
}

#[test]
fn fit_modes_preserve_geometry_and_transparency() {
    let root = tempfile::tempdir().unwrap();
    let path = color_clip(root.path());
    for fit in [ImageFit::Contain, ImageFit::Cover] {
        let stream = VideoStream::new(
            &path,
            4.0,
            (16, 16),
            fit,
            false,
            &PreparationControl::new(false),
        )
        .unwrap();
        assert_eq!(stream.frame().dimensions(), (16, 16));
        assert_eq!(stream.frame().get_pixel(8, 8).0, [50, 0, 0, 255]);
        assert_eq!(
            stream.frame().get_pixel(0, 0).0[3],
            if fit == ImageFit::Contain { 0 } else { 255 }
        );
    }
}

#[test]
fn invalid_and_cancelled_streams_fail_preparation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("broken.mp4");
    fs::write(&path, b"invalid movie").unwrap();
    assert!(VideoStream::new(
        &path,
        30.0,
        (16, 16),
        ImageFit::Stretch,
        true,
        &PreparationControl::new(false)
    )
    .is_err());
    let control = PreparationControl::new(false);
    control.cancel();
    assert!(matches!(
        VideoStream::new(&path, 30.0, (16, 16), ImageFit::Stretch, true, &control),
        Err(MediaError::Cancelled)
    ));
}

#[test]
fn full_screen_video_template_prepares_without_retaining_the_clip() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("background.mp4");
    let result = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=1920x480:rate=30:duration=26.1",
            "-threads",
            "1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let template = serde_json::from_value(serde_json::json!({
        "id": "stream-test", "name": "Streaming", "base_width": 1920, "base_height": 480,
        "background": {"type": "color", "rgb": [0, 0, 0]},
        "widgets": [
            {"id": "video", "x": 960, "y": 240, "width": 1920, "height": 480,
             "kind": {"type": "video", "path": path, "loop_playback": true, "fit": "cover"}},
            {"id": "overlay", "x": 50, "y": 50, "width": 400, "height": 100,
             "kind": {"type": "clock_digital", "format": "%H:%M:%S", "font_size": 48, "color": [255, 255, 255, 255]}}
        ]
    })).unwrap();
    let screen = lianli_shared::screen::ScreenInfo {
        width: 1920,
        height: 480,
        ..lianli_shared::screen::ScreenInfo::WIRELESS_LCD
    };
    let asset = crate::CustomAsset::new(&template, 0.0, &screen, &[], false, 30.0, false).unwrap();
    asset
        .render_frame_rgba_with(true, |bytes| {
            assert_eq!(bytes.len(), 1920 * 480 * 4);
            assert!(bytes[0] > 240 && bytes[1] < 10 && bytes[2] < 10);
            assert!(bytes
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200));
        })
        .unwrap()
        .unwrap();
    assert!(!asset.render_frame(true).unwrap().unwrap().data.is_empty());
}
