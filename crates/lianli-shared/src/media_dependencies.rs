use crate::config::LcdConfig;
use crate::media::MediaType;
use crate::template::{LcdTemplate, TemplateBackground, WidgetKind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    File,
    Image,
    Video,
    Gif,
    Font,
}

pub fn stored_lcd_dependencies(config: &LcdConfig) -> Vec<AssetDependency> {
    let owner = format!("LCD[{}]", config.device_id());
    let mut dependencies = Vec::new();
    if let Some(path) = &config.path {
        dependencies.push(AssetDependency {
            path: path.clone(),
            owner: format!("{owner} saved media"),
            kind: match config.media_type {
                MediaType::Image | MediaType::Sensor => AssetKind::Image,
                MediaType::Video => AssetKind::Video,
                MediaType::Gif => AssetKind::Gif,
                _ => AssetKind::File,
            },
        });
    }
    if let Some(path) = config
        .sensor
        .as_ref()
        .and_then(|sensor| sensor.font_path.as_ref())
    {
        dependencies.push(AssetDependency {
            path: path.clone(),
            owner: format!("{owner} saved sensor font"),
            kind: AssetKind::Font,
        });
    }
    dependencies
}

pub fn map_lcd_paths(config: &mut LcdConfig, mut map: impl FnMut(&mut PathBuf)) {
    if let Some(path) = &mut config.path {
        map(path);
    }
    if let Some(path) = config
        .sensor
        .as_mut()
        .and_then(|sensor| sensor.font_path.as_mut())
    {
        map(path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDependency {
    pub path: PathBuf,
    pub owner: String,
    pub kind: AssetKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetAccessIssue {
    pub owner: String,
    pub path: Option<PathBuf>,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetAccessReport {
    pub uid: u32,
    pub checked: usize,
    pub failed: usize,
    pub issues: Vec<AssetAccessIssue>,
}

pub fn validate_dependency_input(
    lcds: &[LcdConfig],
    templates: &[LcdTemplate],
) -> Result<(), String> {
    if lcds.len() > 256 || templates.len() > 1024 {
        return Err(
            "Media preflight supports at most 256 LCD entries and 1024 templates per state file"
                .into(),
        );
    }
    if lcds.iter().any(|lcd| {
        lcd.serial.as_ref().is_some_and(|id| id.len() > 256)
            || lcd.template_id.as_ref().is_some_and(|id| id.len() > 256)
    }) || templates.iter().any(|template| {
        template.id.len() > 256
            || template.widgets.len() > 1024
            || template.widgets.iter().any(|widget| widget.id.len() > 256)
    }) {
        return Err("Media preflight requires identifiers of at most 256 bytes and at most 1024 widgets per template".into());
    }
    Ok(())
}

pub fn validate_dependency_paths(dependencies: &[AssetDependency]) -> Result<(), String> {
    if dependencies
        .iter()
        .any(|dependency| dependency.path.as_os_str().len() > 4096)
    {
        return Err("Media paths must not exceed 4096 bytes".into());
    }
    Ok(())
}

pub fn template_dependencies(template: &LcdTemplate) -> Vec<AssetDependency> {
    let mut dependencies = Vec::new();
    if let TemplateBackground::Image { path } = &template.background {
        dependencies.push(AssetDependency {
            path: path.clone(),
            owner: format!("Template '{}' background", template.id),
            kind: AssetKind::Image,
        });
    }
    for widget in &template.widgets {
        let owner = format!("Template '{}' widget '{}'", template.id, widget.id);
        let dependency = match &widget.kind {
            WidgetKind::Image { path, .. } => Some((path, AssetKind::Image)),
            WidgetKind::Video { path, .. } => Some((path, AssetKind::Video)),
            kind => kind
                .font_ref()
                .and_then(|font| font.path.as_ref())
                .map(|path| (path, AssetKind::Font)),
        };
        if let Some((path, kind)) = dependency {
            dependencies.push(AssetDependency {
                path: path.clone(),
                owner,
                kind,
            });
        }
    }
    dependencies
}

pub fn lcd_dependencies(
    config: &LcdConfig,
    templates: &[LcdTemplate],
) -> Result<Vec<AssetDependency>, String> {
    let owner = format!("LCD[{}]", config.device_id());
    let mut dependencies = Vec::new();
    match config.media_type {
        MediaType::Image | MediaType::Video | MediaType::Gif => {
            let path = config
                .path
                .clone()
                .ok_or_else(|| format!("{owner} has no media path"))?;
            dependencies.push(AssetDependency {
                path,
                owner,
                kind: if config.media_type == MediaType::Image {
                    AssetKind::Image
                } else if config.media_type == MediaType::Gif {
                    AssetKind::Gif
                } else {
                    AssetKind::Video
                },
            });
        }
        MediaType::Sensor => {
            if let Some(path) = &config.path {
                dependencies.push(AssetDependency {
                    path: path.clone(),
                    owner: format!("{owner} sensor background"),
                    kind: AssetKind::Image,
                });
            }
            if let Some(path) = config
                .sensor
                .as_ref()
                .and_then(|sensor| sensor.font_path.as_ref())
            {
                dependencies.push(AssetDependency {
                    path: path.clone(),
                    owner: format!("{owner} sensor font"),
                    kind: AssetKind::Font,
                });
            }
        }
        MediaType::Custom => {
            let id = config
                .template_id
                .as_deref()
                .ok_or_else(|| format!("{owner} has no template ID"))?;
            let template = templates
                .iter()
                .find(|template| template.id == id)
                .ok_or_else(|| format!("{owner} references missing template '{id}'"))?;
            dependencies = template_dependencies(template);
            for dependency in &mut dependencies {
                dependency.owner = format!("{owner}: {}", dependency.owner);
            }
        }
        MediaType::Color | MediaType::Doublegauge | MediaType::Cooler => {}
    }
    Ok(dependencies)
}

pub fn map_template_paths(template: &mut LcdTemplate, mut map: impl FnMut(&mut PathBuf)) {
    if let TemplateBackground::Image { path } = &mut template.background {
        map(path);
    }
    for widget in &mut template.widgets {
        if let Some(path) = widget.kind.asset_path_mut() {
            map(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_children_and_fonts_are_discovered_and_rewritten_together() {
        let mut template: LcdTemplate = serde_json::from_value(serde_json::json!({
            "id": "custom", "name": "Custom", "base_width": 400, "base_height": 400,
            "background": {"type": "image", "path": "background.png"},
            "widgets": [
                {"id": "image", "x": 0, "y": 0, "width": 100, "height": 100,
                 "kind": {"type": "image", "path": "image.png"}},
                {"id": "video", "x": 0, "y": 0, "width": 100, "height": 100,
                 "kind": {"type": "video", "path": "video.mp4"}},
                {"id": "label", "x": 0, "y": 0, "width": 100, "height": 100,
                 "kind": {"type": "label", "text": "Text", "font_size": 20, "color": [255,255,255], "font": {"path": "font.ttf"}}}
            ]
        })).unwrap();
        let dependencies = template_dependencies(&template);
        assert_eq!(dependencies.len(), 4);
        assert_eq!(dependencies[3].kind, AssetKind::Font);
        assert!(dependencies[3].owner.contains("widget 'label'"));
        map_template_paths(&mut template, |path| {
            *path = PathBuf::from("/managed").join(&*path)
        });
        let rewritten = template_dependencies(&template);
        for (old, new) in dependencies.iter().zip(rewritten) {
            assert_eq!(new.path, PathBuf::from("/managed").join(&old.path));
            assert_eq!(new.owner, old.owner);
        }
    }

    #[test]
    fn missing_templates_are_reported_with_the_configured_lcd() {
        let config = serde_json::from_value(
            serde_json::json!({"type": "custom", "index": 2, "template_id": "missing"}),
        )
        .unwrap();
        let error = lcd_dependencies(&config, &[]).unwrap_err();
        assert!(error.contains("LCD[index:2]"));
        assert!(error.contains("template 'missing'"));
    }

    #[test]
    fn preflight_bounds_identifier_expansion_and_path_bytes() {
        let mut lcd: LcdConfig = serde_json::from_value(serde_json::json!({
            "type": "image", "serial": "a".repeat(256), "path": "x".repeat(4096)
        }))
        .unwrap();
        validate_dependency_input(&[lcd.clone()], &[]).unwrap();
        validate_dependency_paths(&lcd_dependencies(&lcd, &[]).unwrap()).unwrap();
        lcd.serial.as_mut().unwrap().push('a');
        assert!(validate_dependency_input(&[lcd.clone()], &[]).is_err());
        lcd.path = Some("x".repeat(4097).into());
        assert!(validate_dependency_paths(&lcd_dependencies(&lcd, &[]).unwrap()).is_err());
    }
}
