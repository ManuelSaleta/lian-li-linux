use crate::MediaError;

const MAX_FRAMES: usize = 8192;
const MAX_BYTES: usize = 256 * 1024 * 1024;

#[derive(Default)]
pub(super) struct FrameBudget {
    frames: usize,
    bytes: usize,
}

impl FrameBudget {
    pub fn reserve(&mut self, bytes: usize) -> Result<(), MediaError> {
        if self.frames >= MAX_FRAMES || bytes > MAX_BYTES.saturating_sub(self.bytes) {
            return Err(MediaError::InvalidConfig(
                "Animation exceeds 8,192 frames or 256 MiB. Shorten the clip or reduce the video widget size.".into(),
            ));
        }
        self.frames += 1;
        self.bytes += bytes;
        Ok(())
    }
}

pub(super) fn rgba_bytes(width: u32, height: u32) -> Result<usize, MediaError> {
    if width == 0 || height == 0 {
        return Err(MediaError::InvalidConfig(
            "Animation output has no pixels".into(),
        ));
    }
    let mut limits = crate::image::decode_limits();
    limits.check_dimensions(width, height)?;
    limits.reserve_buffer(width, height, image::ColorType::Rgba8)?;
    Ok(width as usize * height as usize * 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_frame_limits_accept_the_boundary_and_reject_the_next_frame() {
        let mut budget = FrameBudget::default();
        budget.reserve(MAX_BYTES).unwrap();
        assert!(budget.reserve(1).is_err());
        let mut budget = FrameBudget::default();
        for _ in 0..MAX_FRAMES {
            budget.reserve(1).unwrap();
        }
        assert!(budget.reserve(1).is_err());
    }

    #[test]
    fn output_geometry_is_checked_before_allocating_a_frame() {
        assert_eq!(rgba_bytes(400, 400).unwrap(), 640_000);
        assert!(rgba_bytes(0, 400).is_err());
        assert!(rgba_bytes(8193, 1).is_err());
        assert!(rgba_bytes(8192, 8192).is_err());
    }
}
