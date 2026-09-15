use super::process::PROBE_TIMEOUT;
use crate::common::MediaError;
use crate::PreparationControl;
use std::path::Path;
use std::process::Command;

/// Probe the source's average frame rate via ffprobe. Returns `None` if the
/// file isn't a video/animated source or ffprobe is unavailable.
pub fn probe_source_fps(path: &Path) -> Option<f32> {
    probe_fps(path, &PreparationControl::new(false))
        .ok()
        .flatten()
}

fn probe_fps(path: &Path, control: &PreparationControl) -> Result<Option<f32>, MediaError> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=avg_frame_rate",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ])
    .arg(path);
    let output = match control.output(cmd, PROBE_TIMEOUT) {
        Ok(output) => output,
        Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
        Err(err) => {
            tracing::warn!("probing source fps failed: {err}");
            control.check()?;
            return Ok(None);
        }
    };
    if !output.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let s = s.trim();
    Ok(s.split_once('/').and_then(|(num, den)| {
        let num: f32 = num.parse().ok()?;
        let den: f32 = den.parse().ok()?;
        (num.is_finite() && den.is_finite() && den > 0.0 && num > 0.0).then_some(num / den)
    }))
}

pub(crate) fn cap_fps_cancellable(
    path: &Path,
    target: f32,
    control: &PreparationControl,
) -> Result<f32, MediaError> {
    control.check()?;
    let target = target.max(1.0);
    Ok(match probe_fps(path, control)? {
        Some(source) if source >= 1.0 => target.min(source),
        _ => target,
    })
}

/// Cap `target` by the source's native fps (when probeable). Always at least 1.
pub fn cap_fps_to_source(path: &Path, target: f32) -> f32 {
    let target = target.max(1.0);
    match probe_source_fps(path) {
        Some(src) if src >= 1.0 => target.min(src),
        _ => target,
    }
}

pub(super) fn stream_rgba(
    input: &Path,
    fps: f32,
    width: u32,
    height: u32,
    control: &PreparationControl,
    consume: impl FnMut(&[u8]) -> Result<(), MediaError>,
) -> Result<(), MediaError> {
    if !fps.is_finite() || fps < 1.0 {
        return Err(MediaError::InvalidFps);
    }
    let frame_bytes = super::frame_budget::rgba_bytes(width, height)?;
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
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
        if control.hardware_video {
            "auto"
        } else {
            "none"
        },
        "-max_pixels",
        "16777216",
        "-i",
    ]);
    command.arg(input).args([
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-vf",
        &format!("scale={width}:{height}:flags=lanczos"),
        "-r",
        &fps.to_string(),
        "-pix_fmt",
        "rgba",
        "-threads",
        "1",
        "-c:v",
        "rawvideo",
        "-f",
        "rawvideo",
        "pipe:1",
    ]);
    let output = control.stream_frames(command, frame_bytes, consume)?;
    if !output.status.success() {
        return Err(MediaError::Ffmpeg(format!(
            "ffmpeg exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}
