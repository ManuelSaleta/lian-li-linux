use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn decode(root: &Path, file: File, kind: &str) -> lianli_control::command::Output {
    let binary = File::open(env!("CARGO_BIN_EXE_lianli-daemon")).unwrap();
    let mut command = Command::new(format!("/proc/self/fd/{}", binary.as_raw_fd()));
    if unsafe { libc::geteuid() } == 0 {
        unsafe {
            command.pre_exec(|| {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setresgid(65534, 65534, 65534) != 0
                    || libc::setresuid(65534, 65534, 65534) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
        .env_remove("CONTAINER_ID")
        .env("container", "decode-test-forbids-service-startup")
        .arg("--config")
        .arg(root.join("missing/config.json"))
        .arg("--socket")
        .arg(root.join("socket"))
        .args(["check-media-decode", "--kind", kind]);
    if kind == "image" {
        command.args(["--extension", "png"]);
    }
    lianli_control::command::run_with_stdin(command, Stdio::from(file), Duration::from_secs(25))
        .unwrap()
}

#[test]
fn isolated_decode_uses_the_open_descriptor_without_starting_hardware_or_writing_state() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    image::RgbImage::from_pixel(2, 3, image::Rgb([40, 50, 60]))
        .save_with_format(&source, image::ImageFormat::Png)
        .unwrap();
    let image = File::open(&source).unwrap();
    let video = File::open(&source).unwrap();
    fs::remove_file(&source).unwrap();
    for (file, kind) in [(image, "image"), (video, "video")] {
        let output = decode(root.path(), file, kind);
        assert!(output.status.success(), "{kind}: {}", output.stderr);
        assert!(output.stdout.is_empty());
    }
    let font = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf");
    let output = decode(root.path(), File::open(font).unwrap(), "font");
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn readable_invalid_media_and_mutable_descriptors_cannot_pass_validation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::write(&source, b"readable but undecodable").unwrap();
    for kind in ["image", "font", "video"] {
        let output = decode(root.path(), File::open(&source).unwrap(), kind);
        assert!(!output.status.success(), "{kind}");
    }
    let output = decode(
        root.path(),
        File::options()
            .read(true)
            .write(true)
            .open(&source)
            .unwrap(),
        "file",
    );
    assert!(!output.status.success());
    assert!(output.stderr.contains("read-only"));
    assert_eq!(fs::read(&source).unwrap(), b"readable but undecodable");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn software_video_validation_requires_a_decoded_frame_and_accepts_gif_and_h264() {
    let root = tempfile::tempdir().unwrap();
    let image = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
    let gif = root.path().join("animation.gif");
    image.save(&gif).unwrap();
    let source = root.path().join("source.png");
    image.save(&source).unwrap();
    let movie = root.path().join("movie.mkv");
    let mut command = Command::new("/usr/bin/ffmpeg");
    command
        .args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-i",
        ])
        .arg(&source)
        .args(["-c:v", "libx264", "-threads", "1", "-pix_fmt", "yuv420p"])
        .arg(&movie);
    let output = lianli_control::command::run(command, Duration::from_secs(20)).unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    for path in [&gif, &movie] {
        let output = decode(root.path(), File::open(path).unwrap(), "video");
        assert!(output.status.success(), "{}", output.stderr);
    }
    let output = decode(root.path(), File::open(&gif).unwrap(), "gif");
    assert!(output.status.success(), "{}", output.stderr);
    let output = decode(root.path(), File::open(&source).unwrap(), "gif");
    assert!(!output.status.success());
    fs::write(&gif, b"GIF89a\x04\x00\x04\x00\x00\x00\x00\x3b").unwrap();
    let output = decode(root.path(), File::open(gif).unwrap(), "video");
    assert!(!output.status.success());
    assert!(!root.path().join("missing").exists());
}
