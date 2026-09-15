use crate::MediaError;

pub(crate) const MAX_TEXT_INPUT_BYTES: usize = 4096;
pub(crate) const MAX_DECIMAL_PLACES: usize = 64;

pub(crate) fn validate_font(
    font: &ab_glyph::FontVec,
    text: &str,
    size: f32,
) -> Result<(), MediaError> {
    use ab_glyph::Font;
    validate(text, size)?;
    if size <= 0.0 {
        return Ok(());
    }
    let mut checked = std::collections::HashSet::new();
    for character in text.chars() {
        let id = font.glyph_id(character);
        if checked.insert(id.0) {
            if let Some(outline) = font.outline_glyph(id.with_scale(size)) {
                if !crate::text_raster::valid_dimensions(outline.px_bounds()) {
                    return Err(MediaError::InvalidConfig("Font glyph exceeds the raster limit at this text size. Reduce the font size or choose another font.".into()));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn format_clock<Tz: chrono::TimeZone>(
    now: &chrono::DateTime<Tz>,
    format: &str,
) -> Result<String, MediaError>
where
    Tz::Offset: std::fmt::Display,
{
    validate(format, 1.0)?;
    struct Output(String);
    impl std::fmt::Write for Output {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            if value.len() > MAX_TEXT_INPUT_BYTES - self.0.len() {
                return Err(std::fmt::Error);
            }
            self.0.push_str(value);
            Ok(())
        }
    }
    let mut output = Output(String::new());
    // Display builds an intermediate String; write_to enforces the bound during expansion.
    now.format(format).write_to(&mut output).map_err(|_| {
        MediaError::InvalidConfig(
            "Clock format is invalid or its expanded text exceeds 4096 UTF-8 bytes".into(),
        )
    })?;
    Ok(output.0)
}

pub(crate) fn validate(text: &str, size: f32) -> Result<(), MediaError> {
    if text.len() > MAX_TEXT_INPUT_BYTES {
        return Err(MediaError::InvalidConfig(
            "Text input exceeds 4096 UTF-8 bytes".into(),
        ));
    }
    if !size.is_finite() || size > 8192.0 {
        return Err(MediaError::InvalidConfig(
            "Text size must be finite and at most 8192 pixels after scaling".into(),
        ));
    }
    Ok(())
}

pub(crate) fn precision(spec: &str) -> Option<usize> {
    let digits = spec.strip_prefix(":.")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse().unwrap_or(usize::MAX))
}

pub(crate) fn validate_format(format: &str) -> Result<(), MediaError> {
    validate(format, 1.0)?;
    if let Some(open) = format.find('{') {
        if let Some(close) = format[open..].find('}') {
            if precision(&format[open + 1..open + close])
                .is_some_and(|value| value > MAX_DECIMAL_PLACES)
            {
                return Err(MediaError::InvalidConfig(
                    "Numeric text precision exceeds 64 decimal places".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_preparation_rejects_oversized_rasters_before_drawing() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf");
        let font = crate::fonts::load(&path).unwrap();
        validate_font(&font, "Temperature 42.5°C", 64.0).unwrap();
        validate("█", 8192.0).unwrap();
        let error = validate_font(&font, "█", 8192.0).unwrap_err();
        assert!(error
            .to_string()
            .contains("Font glyph exceeds the raster limit"));
        validate_font(&font, "M", 0.0).unwrap();
    }

    #[test]
    fn clock_expansion_is_bounded_and_preserves_normal_formats() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-13T21:45:11+03:00").unwrap();
        assert_eq!(
            format_clock(&now, "%H:%M:%S · %Y-%m-%d").unwrap(),
            "21:45:11 · 2026-09-13"
        );
        for format in ["bad %", "%Q", "prefix %Q"] {
            assert!(format_clock(&now, format).is_err());
        }
        assert_eq!(format_clock(&now, &"é".repeat(2048)).unwrap().len(), 4096);
        assert!(format_clock(&now, &"%c".repeat(1000)).is_err());
        assert_eq!(
            format_clock(&now, "%Z").unwrap(),
            now.format("%Z").to_string()
        );
    }

    #[test]
    fn text_and_numeric_precision_limits_reject_oversized_input() {
        assert!(validate(&"é".repeat(2048), 8192.0).is_ok());
        assert!(validate(&"é".repeat(2049), 12.0).is_err());
        assert!(validate("normal", f32::MAX).is_err());
        assert!(validate_format("{:.64} °C").is_ok());
        for format in [
            "{:.65}",
            "{:.1000000000}",
            "{:.999999999999999999999999999999}",
        ] {
            assert!(validate_format(format).is_err());
        }
        assert!(validate_format("{:.2} °C").is_ok());
    }
}
