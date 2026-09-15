use crate::MediaError;
use lianli_shared::template::{LcdTemplate, Widget, WidgetKind};

pub(super) fn widget_origin(
    widget: &Widget,
    scale: f32,
    offset: (i32, i32),
    size: (u32, u32),
) -> (i64, i64) {
    let coordinate = |value: f32, offset: i32, extent: u32| {
        ((value * scale).round() as i64)
            .saturating_add(offset as i64)
            .saturating_sub(extent as i64 / 2)
    };
    (
        coordinate(widget.x, offset.0, size.0),
        coordinate(widget.y, offset.1, size.1),
    )
}

pub(super) fn supersampling(kind: &WidgetKind, smooth_edges: bool) -> u32 {
    let enabled = match kind {
        WidgetKind::RadialGauge { .. }
        | WidgetKind::Speedometer { .. }
        | WidgetKind::Sparkline { .. }
        | WidgetKind::ClockAnalog { .. } => true,
        WidgetKind::VerticalBar { corner_radius, .. }
        | WidgetKind::HorizontalBar { corner_radius, .. } => *corner_radius > 0.1,
        _ => false,
    };
    if smooth_edges && enabled {
        2
    } else {
        1
    }
}

fn check_buffer(width: u32, height: u32) -> Result<(), MediaError> {
    if width == 0 || height == 0 {
        return Err(MediaError::InvalidConfig(
            "Template render buffer has no pixels".into(),
        ));
    }
    let mut limits = crate::image::decode_limits();
    limits.check_dimensions(width, height)?;
    limits.reserve_buffer(width, height, image::ColorType::Rgba8)?;
    Ok(())
}

pub(super) fn clock_number_size(font_size: f32) -> Result<(u32, u32), MediaError> {
    if !font_size.is_finite() {
        return Err(MediaError::InvalidConfig(
            "Invalid clock number size".into(),
        ));
    }
    let size = (
        (font_size * 2.0).max(16.0) as u32,
        (font_size * 1.4).max(16.0) as u32,
    );
    check_buffer(size.0, size.1)?;
    Ok(size)
}

pub(super) fn validate(
    template: &LcdTemplate,
    canvas_w: u32,
    canvas_h: u32,
    smooth_edges: bool,
) -> Result<(f32, u32, u32), MediaError> {
    if template.base_width == 0 || template.base_height == 0 || canvas_w == 0 || canvas_h == 0 {
        return Err(MediaError::InvalidConfig(
            "Template dimensions must be positive".into(),
        ));
    }
    let scale = (canvas_w as f32 / template.base_width as f32)
        .min(canvas_h as f32 / template.base_height as f32)
        .max(0.01);
    let scaled_w = (template.base_width as f32 * scale).round() as u32;
    let scaled_h = (template.base_height as f32 * scale).round() as u32;
    check_buffer(canvas_w, canvas_h)?;
    check_buffer(scaled_w, scaled_h)?;
    for widget in &template.widgets {
        let text_scale = scale * supersampling(&widget.kind, smooth_edges) as f32;
        validate_text(&widget.kind, text_scale).map_err(|error| {
            MediaError::InvalidConfig(format!(
                "Template '{}' widget '{}' text: {error}",
                template.id, widget.id
            ))
        })?;
        if let WidgetKind::ClockAnalog {
            numbers_font_size,
            show_numbers: true,
            numbers_color,
            ..
        } = &widget.kind
        {
            if numbers_color[3] > 0 {
                clock_number_size(*numbers_font_size).map_err(|error| {
                    MediaError::InvalidConfig(format!(
                        "Template '{}' widget '{}' clock numbers: {error}",
                        template.id, widget.id
                    ))
                })?;
            }
        }
        let valid = widget.width.is_finite()
            && widget.height.is_finite()
            && widget.width > 0.0
            && widget.height > 0.0;
        let (width, height) = super::helpers::widget_size_px(widget, scale);
        let factor = supersampling(&widget.kind, smooth_edges);
        let size = width.checked_mul(factor).zip(height.checked_mul(factor));
        let result = match size {
            Some((width, height)) if valid => check_buffer(width, height),
            _ => Err(MediaError::InvalidConfig(
                "Invalid widget dimensions".into(),
            )),
        };
        result.map_err(|error| {
            MediaError::InvalidConfig(format!(
                "Template '{}' widget '{}' render dimensions: {error}",
                template.id, widget.id
            ))
        })?;
    }
    Ok((scale, scaled_w, scaled_h))
}

pub(super) fn validate_font(
    kind: &WidgetKind,
    scale: f32,
    font: &ab_glyph::FontVec,
) -> Result<(), MediaError> {
    let check =
        |text: &str, size: f32| crate::text_validation::validate_font(font, text, size * scale);
    match kind {
        WidgetKind::Label {
            text, font_size, ..
        } => check(text, *font_size),
        WidgetKind::ValueText {
            format,
            unit,
            font_size,
            ..
        } => {
            check(format, *font_size)?;
            check(unit, *font_size)?;
            check("0123456789.-+eNaInf", *font_size)
        }
        WidgetKind::ClockDigital {
            format, font_size, ..
        } => {
            check(format, *font_size)?;
            check(
                "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ:+-., /",
                *font_size,
            )
        }
        WidgetKind::ClockAnalog {
            numbers_font_size,
            show_numbers: true,
            ..
        } => check("0123456789", *numbers_font_size),
        WidgetKind::Sparkline {
            axis_label_format,
            axis_label_size,
            show_axis_labels: true,
            ..
        } => {
            check(axis_label_format, *axis_label_size)?;
            check("0123456789.-+eNaInf", *axis_label_size)
        }
        _ => Ok(()),
    }
}

fn validate_text(kind: &WidgetKind, scale: f32) -> Result<(), MediaError> {
    use crate::text_validation::{validate, validate_format};
    match kind {
        WidgetKind::Label {
            text,
            font_size,
            letter_spacing,
            ..
        } => {
            validate(text, font_size * scale)?;
            validate("", letter_spacing.abs() * scale)?;
        }
        WidgetKind::ValueText {
            format,
            unit,
            font_size,
            letter_spacing,
            ..
        } => {
            validate_format(format)?;
            validate(unit, font_size * scale)?;
            validate("", letter_spacing.abs() * scale)?;
        }
        WidgetKind::ClockDigital {
            format,
            font_size,
            letter_spacing,
            ..
        } => {
            validate(format, font_size * scale)?;
            crate::text_validation::format_clock(&chrono::Local::now(), format)?;
            validate("", letter_spacing.abs() * scale)?;
        }
        WidgetKind::ClockAnalog {
            numbers_font_size,
            show_numbers: true,
            ..
        } => validate("", numbers_font_size * scale)?,
        WidgetKind::Sparkline {
            axis_label_format,
            axis_label_size,
            show_axis_labels: true,
            ..
        } => {
            validate_format(axis_label_format)?;
            validate("", axis_label_size * scale)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_text_fails_preparation_before_font_or_asset_access() {
        let mut template = template();
        template.widgets[0].kind = serde_json::from_value(serde_json::json!({
            "type": "label", "text": "x".repeat(4097), "font_size": 18.0,
            "font": {"path": "/nonexistent/text-fixture.ttf"}, "color": [255,255,255,255]
        }))
        .unwrap();
        let error = crate::custom::CustomAsset::new(
            &template,
            0.0,
            &lianli_shared::screen::ScreenInfo::WIRELESS_LCD,
            &[],
            false,
            30.0,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("widget 'child' text"), "{error}");
        assert!(error.contains("4096"), "{error}");
    }

    #[test]
    fn invalid_clock_format_identifies_the_widget() {
        let mut template = template();
        template.widgets[0].kind = serde_json::from_value(serde_json::json!({
            "type": "clock_digital", "format": "prefix %Q", "font_size": 18.0,
            "color": [255,255,255,255]
        }))
        .unwrap();
        let error = validate(&template, 400, 400, true).unwrap_err().to_string();
        assert!(error.contains("widget 'child' text"), "{error}");
        assert!(error.contains("Clock format is invalid"), "{error}");
    }

    #[test]
    fn clock_number_buffers_follow_render_limits() {
        assert_eq!(clock_number_size(20.0).unwrap(), (40, 28));
        for size in [f32::MAX, f32::NAN, f32::INFINITY, 8192.0] {
            assert!(clock_number_size(size).is_err());
        }
        let mut template = template();
        template.widgets[0].kind = serde_json::from_value(serde_json::json!({
            "type": "clock_analog", "show_numbers": true, "numbers_font_size": 1e30
        }))
        .unwrap();
        assert!(validate(&template, 400, 400, true)
            .unwrap_err()
            .to_string()
            .contains("Text size must be finite"));
    }

    fn template() -> LcdTemplate {
        serde_json::from_value(serde_json::json!({
            "id": "geometry", "name": "Geometry", "base_width": 400, "base_height": 400,
            "background": {"type": "color", "rgb": [0, 0, 0]},
            "widgets": [{"id": "child", "x": 200, "y": 200, "width": 400, "height": 400,
                "kind": {"type": "image", "path": "/nonexistent/geometry-fixture.png"}}]
        }))
        .unwrap()
    }

    #[test]
    fn distant_widgets_clip_without_coordinate_overflow_or_wrapping_onscreen() {
        let mut template = template();
        let source = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let original = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        for position in [f32::MAX, -f32::MAX, i32::MAX as f32, i32::MIN as f32] {
            template.widgets[0].x = position;
            template.widgets[0].y = position;
            let (x, y) = widget_origin(&template.widgets[0], 1.0, (10, -10), (2, 2));
            let mut destination = original.clone();
            super::super::helpers::fast_overlay(&mut destination, &source, x, y);
            assert_eq!(destination, original);
        }
        let mut destination = original.clone();
        super::super::helpers::fast_overlay(&mut destination, &source, -1, -1);
        assert_eq!(destination.get_pixel(0, 0), source.get_pixel(1, 1));
        assert_eq!(destination.get_pixel(1, 0), original.get_pixel(1, 0));
    }

    #[test]
    fn oversized_widget_is_rejected_before_opening_its_asset() {
        let mut template = template();
        assert_eq!(
            validate(&template, 400, 400, true).unwrap(),
            (1.0, 400, 400)
        );
        for (width, height) in [
            (8193.0, 1.0),
            (8192.0, 8192.0),
            (f32::MAX, 1.0),
            (f32::NAN, 1.0),
        ] {
            template.widgets[0].width = width;
            template.widgets[0].height = height;
            let error = crate::custom::CustomAsset::new(
                &template,
                0.0,
                &lianli_shared::screen::ScreenInfo::WIRELESS_LCD,
                &[],
                true,
                30.0,
                false,
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("widget 'child' render dimensions"));
        }
    }

    #[test]
    fn individually_valid_widgets_cannot_exceed_the_shared_retained_budget() {
        let mut template = template();
        template.widgets[0].width = 4096.0;
        template.widgets[0].height = 4096.0;
        template.widgets = vec![template.widgets[0].clone(); 16];
        for _ in 0..2 {
            let error = crate::custom::CustomAsset::new(
                &template,
                0.0,
                &lianli_shared::screen::ScreenInfo::WIRELESS_LCD,
                &[],
                false,
                30.0,
                false,
            )
            .unwrap_err();
            assert!(error.to_string().contains("shared 1 GiB memory limit"));
        }
    }

    #[test]
    fn smoothing_and_scaled_backgrounds_obey_buffer_limits() {
        let mut template = template();
        template.widgets[0].kind = serde_json::from_value(serde_json::json!({
            "type": "sparkline", "source": lianli_shared::media::SensorSourceConfig::CpuUsage,
            "value_min": 0, "value_max": 100, "background_color": [0, 0, 0]
        }))
        .unwrap();
        template.widgets[0].width = 4096.0;
        template.widgets[0].height = 4096.0;
        validate(&template, 400, 400, false).unwrap();
        assert!(validate(&template, 400, 400, true).is_err());
        template.widgets.clear();
        template.base_width = u32::MAX;
        assert!(validate(&template, 400, 400, false).is_err());
        template.base_width = 0;
        assert!(validate(&template, 400, 400, false).is_err());
        template.base_width = 1;
        template.base_height = 8192;
        assert!(validate(&template, 400, 400, false).is_err());
    }
}
