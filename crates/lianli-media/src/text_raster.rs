use ab_glyph::{point, Font, FontVec, PxScale, Rect, ScaleFont};
use image::{Pixel, Rgba, RgbaImage};

// ab_glyph's rasterizer adds four guard floats to its pixel buffer.
const MAX_GLYPH_PIXELS: usize = 64 * 1024 * 1024 / std::mem::size_of::<f32>() - 4;

pub(crate) fn valid_dimensions(bounds: Rect) -> bool {
    let width = bounds.max.x - bounds.min.x;
    let height = bounds.max.y - bounds.min.y;
    width.is_finite()
        && height.is_finite()
        && width > 0.0
        && height > 0.0
        && width <= 8192.0
        && height <= 8192.0
        && f64::from(width) * f64::from(height) <= MAX_GLYPH_PIXELS as f64
}

pub(crate) fn visible_bounds(
    bounds: Rect,
    origin: (i64, i64),
    size: (u32, u32),
) -> Option<(i64, i64)> {
    if !valid_dimensions(bounds) {
        return None;
    }
    let left = origin.0.saturating_add(bounds.min.x as i64);
    let top = origin.1.saturating_add(bounds.min.y as i64);
    (left < i64::from(size.0)
        && top < i64::from(size.1)
        && origin.0.saturating_add(bounds.max.x as i64) > 0
        && origin.1.saturating_add(bounds.max.y as i64) > 0)
        .then_some((left, top))
}

pub(crate) fn draw_text_mut(
    image: &mut RgbaImage,
    color: Rgba<u8>,
    x: impl Into<i64>,
    y: impl Into<i64>,
    scale: impl Into<PxScale>,
    font: &FontVec,
    text: &str,
) {
    if !crate::text_work::text(text) {
        return;
    }
    let origin = (x.into(), y.into());
    let scale = scale.into();
    if !scale.x.is_finite() || !scale.y.is_finite() || scale.x <= 0.0 || scale.y <= 0.0 {
        return;
    }
    let scaled = font.as_scaled(scale);
    let mut cursor = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let id = scaled.glyph_id(character);
        let glyph = id.with_scale_and_position(scale, point(cursor, scaled.ascent()));
        cursor += scaled.h_advance(id);
        if let Some(outline) = scaled.outline_glyph(glyph) {
            if let Some(previous) = previous {
                cursor += scaled.kern(id, previous);
            }
            previous = Some(id);
            let Some((left, top)) = visible_bounds(outline.px_bounds(), origin, image.dimensions())
            else {
                continue;
            };
            let bounds = outline.px_bounds();
            if !crate::text_work::raster(
                (bounds.max.x - bounds.min.x) as u64,
                (bounds.max.y - bounds.min.y) as u64,
            ) {
                return;
            }
            outline.draw(|gx, gy, coverage| {
                let x = left.saturating_add(i64::from(gx));
                let y = top.saturating_add(i64::from(gy));
                if x >= 0 && y >= 0 && x < i64::from(image.width()) && y < i64::from(image.height())
                {
                    let mut color = color;
                    color[3] = (f32::from(color[3]) * coverage.clamp(0.0, 1.0)) as u8;
                    image.get_pixel_mut(x as u32, y as u32).blend(&color);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontVec {
        crate::fonts::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf"),
        )
        .unwrap()
    }

    #[test]
    fn clipped_text_matches_imageproc_blending_and_layout() {
        let font = font();
        for text in ["AV To 42 °C", "Åj gW", "", "   "] {
            for (x, y) in [(-12, -5), (0, 0), (60, 20)] {
                let mut expected = RgbaImage::from_pixel(80, 32, Rgba([3, 7, 11, 128]));
                let mut actual = expected.clone();
                let color = Rgba([240, 220, 180, 192]);
                imageproc::drawing::draw_text_mut(&mut expected, color, x, y, 18.5, &font, text);
                let work = crate::text_work::FrameTextWork::begin();
                draw_text_mut(&mut actual, color, x, y, 18.5, &font, text);
                work.check().unwrap();
                assert_eq!(actual, expected, "{text:?} at ({x}, {y})");
            }
        }
    }

    #[test]
    fn extreme_positions_and_glyph_sizes_do_not_allocate_rasters() {
        let font = font();
        let mut image = RgbaImage::from_pixel(8, 8, Rgba([3, 7, 11, 128]));
        let original = image.clone();
        for (x, y, scale) in [
            (i64::MAX, i64::MIN, 18.0),
            (0, 0, 1e20),
            (0, 0, f32::INFINITY),
        ] {
            draw_text_mut(&mut image, Rgba([255; 4]), x, y, scale, &font, "W");
        }
        assert_eq!(image, original);
        assert!(visible_bounds(
            Rect {
                min: point(0.0, 0.0),
                max: point(8192.0, 8192.0)
            },
            (0, 0),
            (8, 8)
        )
        .is_none());
        assert_eq!(
            visible_bounds(
                Rect {
                    min: point(-2.0, -2.0),
                    max: point(4.0, 4.0)
                },
                (0, 0),
                (8, 8)
            ),
            Some((-2, -2))
        );
        let metrics = crate::common::get_exact_text_metrics(&font, "Wjg", PxScale::from(1e20));
        assert_eq!(metrics, (0, 0, 0, 0, 0.0));
    }
}
