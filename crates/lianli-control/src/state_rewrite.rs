use crate::state::StateFile;
use anyhow::{ensure, Context, Result};
use lianli_shared::config::LcdConfig;
use lianli_shared::media_dependencies::{map_lcd_paths, map_template_paths};
use lianli_shared::template::LcdTemplate;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn rewrite(
    files: &[StateFile],
    config_directory: &Path,
    working_directory: &Path,
    paths: &HashMap<PathBuf, PathBuf>,
) -> Result<Vec<StateFile>> {
    ensure!(paths.len() <= 4096, "Too many media path mappings");
    for (source, destination) in paths {
        ensure!(
            source.is_absolute() && destination.is_absolute(),
            "Migration media paths must be absolute"
        );
        ensure!(
            source.as_os_str().len() <= 4096 && destination.as_os_str().len() <= 4096,
            "Migration media paths exceed 4096 bytes"
        );
        ensure!(
            !source.as_os_str().as_encoded_bytes().contains(&0)
                && !destination.as_os_str().as_encoded_bytes().contains(&0),
            "Migration media paths contain a NUL byte"
        );
    }
    let mut result = Vec::new();
    let mut total = 0;
    for file in files {
        let mut document: Value = serde_json::from_slice(&file.bytes)?;
        let original = document.clone();
        if file.relative_path == Path::new("lcd_templates.json") {
            if let Some(templates) = document.get_mut("templates").and_then(Value::as_array_mut) {
                for raw in templates {
                    let mut template: LcdTemplate = serde_json::from_value(raw.clone())?;
                    let before = serde_json::to_value(&template)?;
                    let mut failure = None;
                    map_template_paths(&mut template, |path| {
                        if failure.is_none() {
                            if let Err(error) = rewrite_path(path, working_directory, paths) {
                                failure = Some(error);
                            }
                        }
                    });
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    apply_changes(raw, &before, &serde_json::to_value(&template)?)?;
                }
            }
        } else if file.relative_path != Path::new("rgb_presets.json") {
            let key = if file.relative_path.parent() == Some(Path::new("profiles"))
                || document.get("lcds").is_some()
            {
                "lcds"
            } else {
                "devices"
            };
            if let Some(lcds) = document.get_mut(key).and_then(Value::as_array_mut) {
                for raw in lcds {
                    let mut lcd: LcdConfig = serde_json::from_value(raw.clone())?;
                    let before = serde_json::to_value(&lcd)?;
                    let mut failure = None;
                    map_lcd_paths(&mut lcd, |path| {
                        if failure.is_none() {
                            if let Err(error) = rewrite_path(path, config_directory, paths) {
                                failure = Some(error);
                            }
                        }
                    });
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    apply_changes(raw, &before, &serde_json::to_value(&lcd)?)?;
                }
            }
        }
        let bytes = if document == original {
            file.bytes.clone()
        } else {
            let mut buffer = StateBuffer {
                bytes: Vec::new(),
                limit: (16 * 1024 * 1024).min(64 * 1024 * 1024 - total),
            };
            serde_json::to_writer_pretty(&mut buffer, &document)?;
            buffer.bytes
        };
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "Rewritten {} exceeds 16 MiB",
            file.relative_path.display()
        );
        total += bytes.len();
        ensure!(total <= 64 * 1024 * 1024, "Rewritten state exceeds 64 MiB");
        result.push(StateFile {
            relative_path: file.relative_path.clone(),
            bytes,
        });
    }
    Ok(result)
}

struct StateBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for StateBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("Rewritten state exceeds its size budget"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn rewrite_path(path: &mut PathBuf, base: &Path, paths: &HashMap<PathBuf, PathBuf>) -> Result<()> {
    let source = if path.is_absolute() {
        path.clone()
    } else {
        base.join(&*path)
    };
    *path = paths
        .get(&source)
        .with_context(|| format!("No destination was prepared for {}", source.display()))?
        .clone();
    Ok(())
}

fn apply_changes(raw: &mut Value, before: &Value, after: &Value) -> Result<()> {
    if before == after {
        return Ok(());
    }
    match (before, after) {
        (Value::Object(before), Value::Object(after)) => {
            let raw = raw
                .as_object_mut()
                .context("State shape changed while rewriting paths")?;
            for (key, value) in after {
                let previous = before
                    .get(key)
                    .context("Path rewriting changed the state schema")?;
                if previous != value {
                    apply_changes(
                        raw.get_mut(key)
                            .context("Changed path field is absent from source JSON")?,
                        previous,
                        value,
                    )?;
                }
            }
        }
        (Value::Array(before), Value::Array(after)) => {
            let raw = raw
                .as_array_mut()
                .context("State array changed while rewriting paths")?;
            ensure!(
                before.len() == after.len() && raw.len() == before.len(),
                "Path rewriting changed array length"
            );
            for ((raw, before), after) in raw.iter_mut().zip(before).zip(after) {
                apply_changes(raw, before, after)?;
            }
        }
        (Value::String(_), Value::String(_)) => *raw = after.clone(),
        _ => anyhow::bail!("Path rewriting attempted to change a non-path value"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateSnapshot;
    use serde_json::json;
    use std::fs;

    fn save(root: &Path, name: &str, value: Value) {
        fs::write(root.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn document(files: &[StateFile], name: &str) -> Value {
        serde_json::from_slice(
            &files
                .iter()
                .find(|file| file.relative_path == Path::new(name))
                .unwrap()
                .bytes,
        )
        .unwrap()
    }

    #[test]
    fn rewrites_all_saved_assets_while_preserving_unknown_fields_and_legacy_shapes() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("profiles")).unwrap();
        save(
            root.path(),
            "config.json",
            json!({
                "devices": [{"index": 0, "type": "color", "rgb": [1,2,3], "path": "dormant.mp4",
                    "sensor": {"label":"Dormant", "unit":"%", "source":{"type":"constant","value":50}, "font_path":"dormant.ttf", "future":17},
                    "future_asset":{"path":"unchanged"}}], "future_setting": {"enabled":true}
            }),
        );
        save(
            root.path(),
            "lcd_templates.json",
            json!({"templates":[{
            "id":"custom", "name":"Custom", "base_width":400, "base_height":400,
            "background":{"type":"image", "path":"background.png", "future":"background"},
            "widgets":[{"id":"text", "x":0, "y":0, "width":100, "height":100,
                "kind":{"type":"label", "text":"Text", "font_size":20, "color":[255,255,255],
                    "font":{"path":"label.ttf", "future":"font"}}, "future":"widget"}]
        }], "future":"templates"}),
        );
        save(
            root.path(),
            "profiles/active.json",
            json!({"name":"Saved", "device_id":"offline", "lcds":[
            {"index":0, "type":"image", "path":"profile.png", "future":"lcd"}
        ], "future":"profile"}),
        );
        save(
            root.path(),
            "profiles/empty.json",
            json!({"name":"Empty", "device_id":"offline", "devices":[{"path":"unknown-extension"}]}),
        );
        let preset_bytes = b"[ ]\n";
        fs::write(root.path().join("rgb_presets.json"), preset_bytes).unwrap();
        let snapshot =
            StateSnapshot::read(&root.path().join("config.json"), Path::new("/template-cwd"))
                .unwrap();
        assert_eq!(snapshot.summary.asset_references, 5);
        let mappings = snapshot
            .dependencies
            .iter()
            .map(|dependency| {
                (
                    dependency.path.clone(),
                    Path::new("/managed").join(dependency.path.file_name().unwrap()),
                )
            })
            .collect();
        let prepared = snapshot.rewrite_paths(&mappings).unwrap();
        let config = document(&prepared, "config.json");
        assert!(config.get("lcds").is_none());
        assert_eq!(config["devices"][0]["path"], "/managed/dormant.mp4");
        assert_eq!(
            config["devices"][0]["sensor"]["font_path"],
            "/managed/dormant.ttf"
        );
        assert_eq!(config["devices"][0]["sensor"]["future"], 17);
        assert_eq!(config["devices"][0]["future_asset"]["path"], "unchanged");
        assert_eq!(config["future_setting"]["enabled"], true);
        let _: lianli_shared::config::AppConfig = serde_json::from_value(config).unwrap();
        let templates = document(&prepared, "lcd_templates.json");
        let template = &templates["templates"][0];
        assert_eq!(template["background"]["path"], "/managed/background.png");
        assert_eq!(template["background"]["future"], "background");
        assert_eq!(
            template["widgets"][0]["kind"]["font"]["path"],
            "/managed/label.ttf"
        );
        assert_eq!(template["widgets"][0]["kind"]["font"]["future"], "font");
        assert_eq!(
            template["widgets"][0]["kind"]["color"],
            json!([255, 255, 255])
        );
        assert_eq!(template["widgets"][0]["future"], "widget");
        assert_eq!(templates["future"], "templates");
        let profile = document(&prepared, "profiles/active.json");
        assert_eq!(profile["lcds"][0]["path"], "/managed/profile.png");
        assert_eq!(profile["lcds"][0]["future"], "lcd");
        assert_eq!(profile["future"], "profile");
        assert_eq!(
            document(&prepared, "profiles/empty.json")["devices"][0]["path"],
            "unknown-extension"
        );
        assert_eq!(
            prepared
                .iter()
                .find(|file| file.relative_path == Path::new("rgb_presets.json"))
                .unwrap()
                .bytes,
            preset_bytes
        );
        snapshot
            .verify_unchanged(&root.path().join("config.json"), Path::new("/template-cwd"))
            .unwrap();
    }

    #[test]
    fn missing_or_invalid_destinations_leave_sources_unchanged() {
        let root = tempfile::tempdir().unwrap();
        save(
            root.path(),
            "config.json",
            json!({"lcds":[{"index":0,"type":"image","path":"image.png"}]}),
        );
        let config = root.path().join("config.json");
        let snapshot = StateSnapshot::read(&config, root.path()).unwrap();
        let original = fs::read(&config).unwrap();
        assert!(snapshot
            .rewrite_paths(&HashMap::new())
            .err()
            .unwrap()
            .to_string()
            .contains("No destination"));
        let source = root.path().join("image.png");
        assert!(snapshot
            .rewrite_paths(&HashMap::from([(
                source.clone(),
                PathBuf::from("relative.png")
            )]))
            .is_err());
        assert!(snapshot
            .rewrite_paths(&HashMap::from([(source, PathBuf::from("/invalid\0path"))]))
            .is_err());
        assert_eq!(fs::read(&config).unwrap(), original);
    }

    #[test]
    fn serializer_stops_at_the_budget_before_allocating_the_whole_output() {
        let mut buffer = StateBuffer {
            bytes: Vec::new(),
            limit: 64,
        };
        assert!(
            serde_json::to_writer_pretty(&mut buffer, &json!({"value":"x".repeat(128)})).is_err()
        );
        assert!(buffer.bytes.len() <= 64);
    }
}
