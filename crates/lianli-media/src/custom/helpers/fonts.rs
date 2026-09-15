use crate::common::MediaError;
use ab_glyph::FontVec;
use lianli_shared::template::FontRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn load_font_from_disk(path: &Path) -> Result<FontVec, MediaError> {
    crate::fonts::load(path)
}

pub fn resolve_font<'a>(
    font_ref: &FontRef,
    fonts: &'a HashMap<PathBuf, FontVec>,
    default: &'a FontVec,
) -> &'a FontVec {
    if let Some(p) = &font_ref.path {
        if let Some(f) = fonts.get(p) {
            return f;
        }
    }
    default
}
