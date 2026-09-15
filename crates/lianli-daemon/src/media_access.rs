use anyhow::{ensure, Result};
use lianli_shared::config::LcdConfig;
use lianli_shared::media::MediaType;
use lianli_shared::media_dependencies::{
    lcd_dependencies, template_dependencies, validate_dependency_input, validate_dependency_paths,
    AssetAccessIssue, AssetAccessReport, AssetDependency,
};
use lianli_shared::template::LcdTemplate;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;

const MAX_INPUT: usize = 1024 * 1024;
const MAX_DEPENDENCIES: usize = 1024;
const MAX_ISSUES: usize = 32;

#[derive(Serialize, Deserialize)]
pub struct Input {
    pub lcds: Vec<LcdConfig>,
    pub templates: Vec<LcdTemplate>,
    pub config_directory: PathBuf,
}

impl Input {
    fn check(self) -> Result<AssetAccessReport> {
        validate_dependency_input(&self.lcds, &self.templates).map_err(anyhow::Error::msg)?;
        let mut report = AssetAccessReport {
            uid: unsafe { libc::geteuid() },
            checked: 0,
            failed: 0,
            issues: Vec::new(),
        };
        let referenced: std::collections::HashSet<_> = self
            .lcds
            .iter()
            .filter(|lcd| lcd.media_type == MediaType::Custom)
            .filter_map(|lcd| lcd.template_id.clone())
            .collect();
        for mut lcd in self.lcds {
            lcd.resolve_paths(&self.config_directory);
            let dependencies = match lcd_dependencies(&lcd, &self.templates) {
                Ok(dependencies) => dependencies,
                Err(error) => {
                    record(
                        &mut report,
                        AssetAccessIssue {
                            owner: format!("LCD[{}]", lcd.device_id()),
                            path: None,
                            error,
                        },
                    );
                    continue;
                }
            };
            check_dependencies(&mut report, dependencies)?;
        }
        for template in &self.templates {
            if !referenced.contains(&template.id) {
                check_dependencies(&mut report, template_dependencies(template))?;
            }
        }
        Ok(report)
    }
}

fn check_dependencies(
    report: &mut AssetAccessReport,
    dependencies: Vec<AssetDependency>,
) -> Result<()> {
    validate_dependency_paths(&dependencies).map_err(anyhow::Error::msg)?;
    ensure!(
        report.checked + dependencies.len() <= MAX_DEPENDENCIES,
        "Too many media dependencies to check"
    );
    for dependency in dependencies {
        report.checked += 1;
        if let Err(error) = lianli_media::asset_access::check_file(&dependency.path) {
            record(
                report,
                AssetAccessIssue {
                    owner: dependency.owner,
                    path: Some(dependency.path),
                    error: error.to_string(),
                },
            );
        }
    }
    Ok(())
}

fn record(report: &mut AssetAccessReport, issue: AssetAccessIssue) {
    report.failed += 1;
    if report.issues.len() < MAX_ISSUES {
        report.issues.push(issue);
    }
}

pub fn run_cli() -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_INPUT,
        "Media access request exceeds 1 MiB"
    );
    let input: Input = serde_json::from_slice(&bytes)?;
    serde_json::to_writer(std::io::stdout().lock(), &input.check()?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn resolves_direct_paths_and_reports_each_missing_template_child() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("image.png"), b"readable").unwrap();
        let lcds = serde_json::from_value(json!([
            {"index": 0, "type": "image", "path": "image.png"},
            {"index": 1, "type": "custom", "template_id": "custom"},
            {"index": 2, "type": "custom", "template_id": "absent"}
        ]))
        .unwrap();
        let image = root.path().join("absent.png");
        let font = root.path().join("absent.ttf");
        let templates = serde_json::from_value(json!([{
            "id": "custom", "name": "Custom", "base_width": 400, "base_height": 400,
            "background": {"type": "image", "path": image},
            "widgets": [{"id": "text", "x": 0, "y": 0, "width": 100, "height": 100,
                "kind": {"type": "label", "text": "Text", "font_size": 20, "color": [255,255,255], "font": {"path": font}}}]
        }])).unwrap();
        let report = Input {
            lcds,
            templates,
            config_directory: root.path().into(),
        }
        .check()
        .unwrap();
        assert_eq!(report.checked, 3);
        assert_eq!(report.failed, 3);
        assert_eq!(report.issues[0].path.as_ref(), Some(&image));
        assert!(report.issues[0].owner.contains("background"));
        assert_eq!(report.issues[1].path.as_ref(), Some(&font));
        assert!(report.issues[1].owner.contains("widget 'text'"));
        assert!(report.issues[2].error.contains("missing template 'absent'"));
    }

    #[test]
    fn follows_symlinks_rechecks_loss_and_bounds_issue_lists() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("image.png");
        let link = root.path().join("link.png");
        fs::write(&target, b"readable").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let make_input = || Input {
            lcds: (0..40)
                .map(|index| {
                    serde_json::from_value(json!({"index": index, "type": "image", "path": link}))
                        .unwrap()
                })
                .collect(),
            templates: Vec::new(),
            config_directory: root.path().into(),
        };
        assert_eq!(make_input().check().unwrap().failed, 0);
        fs::remove_file(target).unwrap();
        let report = make_input().check().unwrap();
        assert_eq!(report.checked, 40);
        assert_eq!(report.failed, 40);
        assert_eq!(report.issues.len(), MAX_ISSUES);
    }

    #[test]
    fn resolves_sensor_background_and_font_against_the_configuration_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("background.png"), b"readable").unwrap();
        let lcds = serde_json::from_value(json!([{
            "index": 0, "type": "sensor", "path": "background.png",
            "sensor": { "label": "Sensor", "unit": "%", "source": {"type": "constant", "value": 50}, "font_path": "missing.ttf" }
        }])).unwrap();
        let report = Input {
            lcds,
            templates: Vec::new(),
            config_directory: root.path().into(),
        }
        .check()
        .unwrap();
        assert_eq!(report.checked, 2);
        assert_eq!(report.failed, 1);
        assert_eq!(report.issues[0].path, Some(root.path().join("missing.ttf")));
        assert!(report.issues[0].owner.contains("sensor font"));
    }

    #[test]
    fn checks_unsaved_templates_without_requiring_a_configured_lcd() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("unsaved-background.png");
        let templates = serde_json::from_value(json!([{
            "id": "draft", "name": "Draft", "base_width": 400, "base_height": 400,
            "background": {"type": "image", "path": path}, "widgets": []
        }]))
        .unwrap();
        let report = Input {
            lcds: Vec::new(),
            templates,
            config_directory: root.path().into(),
        }
        .check()
        .unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.issues[0].owner, "Template 'draft' background");
        assert_eq!(report.issues[0].path, Some(path));
    }
}
