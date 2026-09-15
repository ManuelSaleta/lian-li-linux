use crate::{MediaError, PreparationControl};
use lianli_shared::media_dependencies::AssetDependency;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn check_file(path: &Path) -> io::Result<()> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::other("Asset is not a regular file"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Asset is not a regular file"));
    }
    Ok(())
}

pub fn validate_dependencies(
    dependencies: &[AssetDependency],
    control: &PreparationControl,
) -> Result<(), MediaError> {
    for dependency in dependencies {
        control.check()?;
        let result = check_file(&dependency.path);
        result.map_err(|error| MediaError::InvalidConfig(format!(
            "{} cannot read '{}': {error}. The file and its parent directories must be accessible to the daemon account.",
            dependency.owner, dependency.path.display(),
        )))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::media_dependencies::AssetKind;
    use std::os::unix::fs::symlink;

    #[test]
    fn missing_template_child_fails_preparation_instead_of_rendering_a_partial_template() {
        let root = tempfile::tempdir().unwrap();
        let template = serde_json::from_value(serde_json::json!({
            "id": "custom", "name": "Custom", "base_width": 400, "base_height": 400,
            "background": {"type": "color", "rgb": [0,0,0]},
            "widgets": [{"id": "child-video", "x": 0, "y": 0, "width": 100, "height": 100,
                "kind": {"type": "video", "path": root.path().join("missing.mp4")}}]
        }))
        .unwrap();
        let error = crate::CustomAsset::new(
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
        assert!(error.contains("Template 'custom' widget 'child-video'"));
        assert!(error.contains("missing.mp4"));
    }

    #[test]
    fn access_checks_follow_asset_symlinks_and_identify_the_failing_child() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("video.mp4");
        fs::write(&target, b"readability does not validate decoding").unwrap();
        let link = root.path().join("linked.mp4");
        symlink(&target, &link).unwrap();
        let mut dependency = AssetDependency {
            path: link,
            owner: "Template 'custom' widget 'video'".into(),
            kind: AssetKind::Video,
        };
        let control = PreparationControl::new(false);
        assert!(validate_dependencies(&[dependency.clone()], &control).is_ok());
        fs::remove_file(target).unwrap();
        let error = validate_dependencies(&[dependency.clone()], &control)
            .unwrap_err()
            .to_string();
        assert!(error.contains("widget 'video'"));
        assert!(error.contains("linked.mp4"));
        dependency.path = root.path().into();
        assert!(validate_dependencies(&[dependency], &control)
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
    }
}
