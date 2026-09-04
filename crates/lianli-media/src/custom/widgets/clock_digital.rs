//! Digital clock — renders `chrono::Local::now()` via a strftime format string.

use super::super::helpers::draw_text_widget;
use ab_glyph::FontVec;
use chrono::Local;
use image::RgbaImage;
use lianli_shared::template::TextAlign;
use std::fmt::Write as _;

#[allow(clippy::too_many_arguments)]
pub(in super::super) fn draw(
    sub: &mut RgbaImage,
    format: &str,
    font: &FontVec,
    size: f32,
    color: [u8; 4],
    align: TextAlign,
    ww: u32,
    wh: u32,
    letter_spacing: f32,
) {
    let now = Local::now();
    // An invalid strftime string makes DelayedFormat s Display return an
    // error, which plain to_string turns into a panic. Fall back to the
    // default format instead of killing the render thread.
    let mut text = String::new();
    if write!(text, "{}", now.format(format)).is_err() {
        let _ = write!(text, "{}", now.format("%H:%M"));
    }
    draw_text_widget(sub, &text, font, size, color, align, ww, wh, letter_spacing);
}
