use crate::common::get_exact_text_metrics;
use crate::text_raster::draw_text_mut;
use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use lianli_shared::template::TextAlign;

#[allow(clippy::too_many_arguments)]
pub fn draw_text_widget(
    sub: &mut RgbaImage,
    text: &str,
    font: &FontVec,
    size: f32,
    color: [u8; 4],
    align: TextAlign,
    ww: u32,
    wh: u32,
    letter_spacing: f32,
) {
    if !crate::text_work::text(text) {
        return;
    }
    if text.is_empty() || color[3] == 0 || !size.is_finite() || !letter_spacing.is_finite() {
        return;
    }
    let scale = PxScale::from(size.max(1.0));

    if letter_spacing.abs() < f32::EPSILON {
        let (tw, th, ox, oy, _ascent) = get_exact_text_metrics(font, text, scale);
        if tw <= 0 || th <= 0 {
            return;
        }
        let x = match align {
            TextAlign::Left => 0,
            TextAlign::Center => (i64::from(ww) - i64::from(tw)) / 2,
            TextAlign::Right => i64::from(ww) - i64::from(tw),
        } - i64::from(ox);
        let y = (i64::from(wh) - i64::from(th)) / 2 - i64::from(oy);
        draw_text_mut(sub, Rgba(color), x, y, scale, font, text);
        return;
    }

    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let mut cursor_x = 0.0_f32;
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        let advance = scaled.h_advance(glyph_id);
        cursor_x += advance + letter_spacing;
    }
    let total_w = (cursor_x - letter_spacing).max(0.0);
    let th = (ascent - scaled.descent()) as i64;

    let base_x = match align {
        TextAlign::Left => 0.0,
        TextAlign::Center => (ww as f32 - total_w) / 2.0,
        TextAlign::Right => ww as f32 - total_w,
    };
    let base_y = i64::from(wh).saturating_sub(th) / 2;

    let rgba = Rgba(color);
    let (iw, ih) = (i64::from(sub.width()), i64::from(sub.height()));
    cursor_x = 0.0;
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        let glyph = glyph_id.with_scale_and_position(scale, point(cursor_x, ascent));
        cursor_x += scaled.h_advance(glyph_id) + letter_spacing;
        if let Some(outlined) = scaled.outline_glyph(glyph) {
            let bb = outlined.px_bounds();
            let Some((left, top)) = crate::text_raster::visible_bounds(
                bb,
                (base_x.round() as i64, base_y),
                sub.dimensions(),
            ) else {
                continue;
            };
            let bounds = outlined.px_bounds();
            if !crate::text_work::raster(
                (bounds.max.x - bounds.min.x) as u64,
                (bounds.max.y - bounds.min.y) as u64,
            ) {
                return;
            }
            outlined.draw(|gx, gy, gv| {
                if gv <= 0.0 {
                    return;
                }
                let px = left.saturating_add(i64::from(gx));
                let py = top.saturating_add(i64::from(gy));
                if px < 0 || py < 0 || px >= iw || py >= ih {
                    return;
                }
                let a = gv * (color[3] as f32 / 255.0);
                let pix = sub.get_pixel_mut(px as u32, py as u32);
                pix[0] = (pix[0] as f32 * (1.0 - a) + rgba[0] as f32 * a).round() as u8;
                pix[1] = (pix[1] as f32 * (1.0 - a) + rgba[1] as f32 * a).round() as u8;
                pix[2] = (pix[2] as f32 * (1.0 - a) + rgba[2] as f32 * a).round() as u8;
                let alpha_out = pix[3] as f32 / 255.0 + a * (1.0 - pix[3] as f32 / 255.0);
                pix[3] = (alpha_out * 255.0).round().min(255.0) as u8;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_alignment_and_spacing_preserve_raster_pixels() {
        let font = crate::fonts::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf"),
        )
        .unwrap();
        let mut hashes = Vec::new();
        for align in [TextAlign::Left, TextAlign::Center, TextAlign::Right] {
            for spacing in [0.0, 1.5, -0.5] {
                let mut image = RgbaImage::from_pixel(80, 32, Rgba([3, 7, 11, 128]));
                draw_text_widget(
                    &mut image,
                    "42.5 °C abc",
                    &font,
                    18.0,
                    [240, 220, 180, 192],
                    align,
                    80,
                    32,
                    spacing,
                );
                hashes.push(
                    image
                        .as_raw()
                        .iter()
                        .fold(0xcbf29ce484222325u64, |hash, byte| {
                            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                        }),
                );
            }
        }
        assert_eq!(
            hashes,
            vec![
                2534332138333932455,
                30300932541907825,
                3873579789861976303,
                17064109230767084441,
                5226475748987678912,
                1100984886348822106,
                146056972139753317,
                4604477247474879946,
                5565780027399916257
            ]
        );
    }
}
