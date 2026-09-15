use lianli_shared::template::WidgetKind;

pub fn format_sensor_readout(kind: &WidgetKind, raw: f32) -> (String, i32) {
    match kind {
        WidgetKind::ValueText { format, unit, .. } => {
            let text = render_value_format(format, raw);
            let quantized = (raw * 10.0).round() as i32;
            (format!("{text}{unit}"), quantized)
        }
        WidgetKind::RadialGauge {
            value_min,
            value_max,
            ..
        }
        | WidgetKind::VerticalBar {
            value_min,
            value_max,
            ..
        }
        | WidgetKind::HorizontalBar {
            value_min,
            value_max,
            ..
        }
        | WidgetKind::Speedometer {
            value_min,
            value_max,
            ..
        }
        | WidgetKind::Sparkline {
            value_min,
            value_max,
            ..
        } => {
            let span = (value_max - value_min).abs().max(f32::EPSILON);
            let q = (((raw - value_min) / span) * 1000.0).round() as i32;
            (String::new(), q)
        }
        _ => (String::new(), 0),
    }
}

pub fn render_value_format(fmt: &str, value: f32) -> String {
    if fmt.len() > crate::text_validation::MAX_TEXT_INPUT_BYTES {
        return format!("{value:.0}");
    }
    if let Some(open) = fmt.find('{') {
        if let Some(close_rel) = fmt[open..].find('}') {
            let close = open + close_rel;
            let spec = &fmt[open + 1..close];
            let decimals = crate::text_validation::precision(spec)
                .unwrap_or(0)
                .min(crate::text_validation::MAX_DECIMAL_PLACES);
            let prefix = &fmt[..open];
            let suffix = &fmt[close + 1..];
            return format!("{prefix}{:.*}{suffix}", decimals, value);
        }
    }
    format!("{:.0}", value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_precision_remains_bounded_and_normal_formats_match() {
        assert_eq!(render_value_format("{:.2} °C", 42.125), "42.12 °C");
        assert_eq!(render_value_format("plain", 42.0), "42");
        for format in ["{:.1000000000}", "{:.999999999999999999999999999999}"] {
            let rendered = render_value_format(format, 1.0);
            assert_eq!(rendered.len(), 66);
            assert!(rendered.starts_with("1.000"));
        }
        assert_eq!(render_value_format(&"x".repeat(4097), 42.0), "42");
    }
}
