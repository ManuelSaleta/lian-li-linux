use super::super::helpers::blit_with_opacity;
use super::WidgetState;
use image::RgbaImage;

pub(in super::super) fn draw(sub: &mut RgbaImage, state: &WidgetState, opacity: f32) {
    if let Some(stream) = &state.video_stream {
        blit_with_opacity(sub, stream.frame(), opacity);
    }
}
