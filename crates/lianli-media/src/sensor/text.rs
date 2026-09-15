use super::bitmap_glyphs::glyph_pattern;
use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use image::{Rgb, RgbImage};

pub(super) struct TextRenderParams<'a> {
    pub label: &'a str,
    pub unit: &'a str,
    pub color: [u8; 3],
    pub value_size: f32,
    pub unit_size: f32,
    pub label_size: f32,
    pub value_offset: i32,
    pub unit_offset: i32,
    pub label_offset: i32,
    pub value_text: &'a str,
}

pub(super) fn draw_sensor_text_ttf(
    image: &mut RgbImage,
    width: u32,
    height: u32,
    params: TextRenderParams,
    font: &FontVec,
) {
    draw_text_centered(
        image,
        (width, height),
        params.value_text,
        params.value_size,
        params.color,
        params.value_offset,
        font,
    );
    draw_text_centered(
        image,
        (width, height),
        params.unit,
        params.unit_size,
        params.color,
        params.unit_offset,
        font,
    );
    draw_text_centered(
        image,
        (width, height),
        params.label,
        params.label_size,
        params.color,
        params.label_offset,
        font,
    );
}

fn draw_text_centered(
    image: &mut RgbImage,
    (width, height): (u32, u32),
    text: &str,
    size: f32,
    color: [u8; 3],
    offset_y: i32,
    font: &FontVec,
) {
    if !crate::text_work::text(text) {
        return;
    }
    let width = width.min(image.width());
    let height = height.min(image.height());
    if !size.is_finite() || size <= 0.0 || text.is_empty() || width == 0 || height == 0 {
        return;
    }

    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);

    let mut cursor_x = 0.0_f32;
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        cursor_x += scaled.h_advance(glyph_id);
    }

    let text_width = cursor_x;
    let start_x = ((width as f32 - text_width) / 2.0) as i64;
    let start_y = i64::from(height) / 2 + i64::from(offset_y);

    cursor_x = 0.0;
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        let glyph = glyph_id.with_scale_and_position(scale, point(cursor_x, scaled.ascent()));
        cursor_x += scaled.h_advance(glyph_id);
        if let Some(outlined) = scaled.outline_glyph(glyph) {
            let bb = outlined.px_bounds();
            let Some((left, top)) =
                crate::text_raster::visible_bounds(bb, (start_x, start_y), (width, height))
            else {
                continue;
            };
            if !crate::text_work::raster((bb.max.x - bb.min.x) as u64, (bb.max.y - bb.min.y) as u64)
            {
                return;
            }
            outlined.draw(|gx, gy, gv| {
                let x = left.saturating_add(i64::from(gx));
                let y = top.saturating_add(i64::from(gy));
                if x >= 0 && x < i64::from(width) && y >= 0 && y < i64::from(height) {
                    let px = image.get_pixel_mut(x as u32, y as u32);
                    let alpha = gv;
                    px.0[0] = ((color[0] as f32 * alpha) + (px.0[0] as f32 * (1.0 - alpha))) as u8;
                    px.0[1] = ((color[1] as f32 * alpha) + (px.0[1] as f32 * (1.0 - alpha))) as u8;
                    px.0[2] = ((color[2] as f32 * alpha) + (px.0[2] as f32 * (1.0 - alpha))) as u8;
                }
            });
        }
    }
}

pub(super) fn draw_sensor_text_fallback(
    image: &mut RgbImage,
    width: u32,
    height: u32,
    params: TextRenderParams,
) {
    let value_scale = (params.value_size / 4.0).max(4.0) as u32;
    let unit_scale = (params.unit_size / 4.0).max(3.0) as u32;
    let label_scale = (params.label_size / 4.0).max(3.0) as u32;

    draw_text_center_bitmap(
        image,
        width,
        height,
        params.value_text,
        value_scale,
        params.color,
        params.value_offset,
    );
    draw_text_center_bitmap(
        image,
        width,
        height,
        params.unit,
        unit_scale,
        params.color,
        params.unit_offset,
    );
    draw_text_center_bitmap(
        image,
        width,
        height,
        params.label,
        label_scale,
        params.color,
        params.label_offset,
    );
}

fn draw_text_center_bitmap(
    image: &mut RgbImage,
    width: u32,
    height: u32,
    text: &str,
    scale: u32,
    color: [u8; 3],
    offset_y: i32,
) {
    if !crate::text_work::text(text) {
        return;
    }
    let width = width.min(image.width());
    let height = height.min(image.height());
    if scale == 0 || width == 0 || height == 0 {
        return;
    }
    let step = 6 * u64::from(scale);
    let measured_glyphs = (u64::from(width) + u64::from(scale)) / step + 1;
    let glyph_count = text.chars().take(measured_glyphs as usize).count();
    if glyph_count == 0 {
        return;
    }
    let total_width = (glyph_count as u64 * step - u64::from(scale)).min(u64::from(width));
    let start_x = (u64::from(width) - total_width) as i64 / 2;
    let start_y = i64::from(height) / 2 + i64::from(offset_y) - 7 * i64::from(scale) / 2;

    for (i, character) in text.chars().take(glyph_count).enumerate() {
        let base_x = start_x + i as i64 * step as i64;
        if base_x >= i64::from(width) {
            break;
        }
        if !crate::text_work::raster(
            u64::from(width).min(5 * u64::from(scale)),
            u64::from(height).min(7 * u64::from(scale)),
        ) {
            return;
        }
        draw_bitmap_character(
            image,
            (width, height),
            (base_x, start_y),
            glyph_pattern(character),
            scale,
            color,
        );
    }
}

fn draw_bitmap_character(
    image: &mut RgbImage,
    (width, height): (u32, u32),
    (base_x, base_y): (i64, i64),
    bitmap: [u8; 7],
    scale: u32,
    color: [u8; 3],
) {
    for (row, mask) in bitmap.iter().enumerate() {
        for col in 0..5 {
            if (mask >> (4 - col)) & 1 == 1 {
                let left = base_x + i64::from(col) * i64::from(scale);
                let top = base_y + row as i64 * i64::from(scale);
                let right = (left + i64::from(scale)).min(i64::from(width));
                let bottom = (top + i64::from(scale)).min(i64::from(height));
                for y in top.max(0)..bottom {
                    for x in left.max(0)..right {
                        image.put_pixel(x as u32, y as u32, Rgb(color));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttf_text_clips_extreme_offsets_without_overflow() {
        let font = crate::fonts::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf"),
        )
        .unwrap();
        let mut image = RgbImage::from_pixel(32, 32, Rgb([3, 7, 11]));
        let original = image.clone();
        for offset in [i32::MIN, i32::MAX] {
            draw_text_centered(
                &mut image,
                (u32::MAX, u32::MAX),
                "42 °C",
                18.0,
                [255; 3],
                offset,
                &font,
            );
        }
        assert_eq!(image, original);
        draw_text_centered(&mut image, (32, 32), "42 °C", 18.0, [255; 3], -4, &font);
        assert_ne!(image, original);
    }

    #[test]
    fn bitmap_text_bounds_work_for_extreme_scale_offsets_and_long_input() {
        let mut image = RgbImage::new(8, 8);
        draw_text_center_bitmap(&mut image, 8, 8, "8", u32::MAX, [255, 0, 0], i32::MAX);
        draw_text_center_bitmap(&mut image, 8, 8, "8", u32::MAX, [255, 0, 0], i32::MIN);
        let mut expected = RgbImage::new(80, 32);
        draw_text_center_bitmap(&mut expected, 80, 32, &"8".repeat(20), 2, [255, 0, 0], 0);
        let mut actual = RgbImage::new(80, 32);
        draw_text_center_bitmap(
            &mut actual,
            80,
            32,
            &"8".repeat(1_000_000),
            2,
            [255, 0, 0],
            0,
        );
        assert_eq!(actual, expected);
        let mut filled = RgbImage::new(4, 4);
        draw_bitmap_character(
            &mut filled,
            (4, 4),
            (-2, -2),
            [31; 7],
            u32::MAX,
            [7, 11, 19],
        );
        assert!(filled.pixels().all(|pixel| pixel.0 == [7, 11, 19]));
    }

    #[test]
    fn sensor_text_preserves_ttf_and_bitmap_pixels() {
        let font = crate::fonts::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf"),
        )
        .unwrap();
        let mut hashes = Vec::new();
        for offset in [-5, 5] {
            let mut image = RgbImage::from_pixel(80, 32, Rgb([3, 7, 11]));
            draw_text_centered(
                &mut image,
                (80, 32),
                "42.5 °C abc",
                18.0,
                [240, 220, 180],
                offset,
                &font,
            );
            hashes.push(
                image
                    .as_raw()
                    .iter()
                    .fold(0xcbf29ce484222325u64, |hash, byte| {
                        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                    }),
            );
            let mut image = RgbImage::from_pixel(80, 32, Rgb([3, 7, 11]));
            draw_text_center_bitmap(&mut image, 80, 32, "42.5 C abc", 2, [240, 220, 180], offset);
            hashes.push(
                image
                    .as_raw()
                    .iter()
                    .fold(0xcbf29ce484222325u64, |hash, byte| {
                        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                    }),
            );
        }
        assert_eq!(
            hashes,
            vec![
                3676089542022841177,
                5441611535783272777,
                11566977614713818520,
                12761253909401323977
            ]
        );
    }
}
