use crate::MediaError;
use ab_glyph::FontVec;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn load(path: &Path) -> Result<FontVec, MediaError> {
    let result = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(MediaError::from)
        .and_then(decode);
    result.map_err(|error| {
        MediaError::Sensor(format!("font '{}' load failed: {error}", path.display()))
    })
}

pub(crate) fn decode(file: File) -> Result<FontVec, MediaError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(MediaError::InvalidConfig(
            "Custom font must be a regular file".into(),
        ));
    }
    if metadata.len() > MAX_FONT_BYTES {
        return Err(MediaError::InvalidConfig(
            "Custom font exceeds 32 MiB".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FONT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FONT_BYTES {
        return Err(MediaError::InvalidConfig(
            "Custom font exceeds 32 MiB".into(),
        ));
    }
    FontVec::try_from_vec(bytes)
        .map_err(|error| MediaError::Sensor(format!("Parsing custom font: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_preserve_glyphs_through_runtime_and_preflight() {
        use ab_glyph::Font;
        for relative in [
            "templates/assets/neon-us88/JetBrainsMonoNL-Medium.ttf",
            "templates/assets/lancool207-a5/NotoSansTC-Regular.otf",
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(relative);
            let font = load(&path).unwrap();
            assert_ne!(font.glyph_id('A').0, 0);
            crate::validation::check_still(
                File::open(path).unwrap(),
                lianli_shared::media_dependencies::AssetKind::Font,
                None,
            )
            .unwrap();
        }
    }

    #[test]
    fn runtime_fonts_reject_oversized_and_nonregular_sources() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oversized.ttf");
        File::create(&path)
            .unwrap()
            .set_len(MAX_FONT_BYTES + 1)
            .unwrap();
        assert!(load(&path).unwrap_err().to_string().contains("32 MiB"));
        assert!(load(root.path())
            .unwrap_err()
            .to_string()
            .contains("regular file"));
        std::fs::write(&path, b"invalid font").unwrap();
        assert!(load(&path).unwrap_err().to_string().contains("Parsing"));
    }
}
