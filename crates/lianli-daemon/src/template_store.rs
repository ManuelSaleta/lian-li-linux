//! Persistence for LCD templates.

use anyhow::{Context, Result};
use lianli_shared::sensors::SensorInfo;
use lianli_shared::template::LcdTemplate;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct TemplateFile {
    #[serde(default, deserialize_with = "lianli_shared::serde_limits::templates")]
    templates: Vec<LcdTemplate>,
}

pub fn templates_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lcd_templates.json")
}

pub fn read_user_templates(path: &Path) -> Result<Vec<LcdTemplate>> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "{} is not a regular template file",
        path.display()
    );
    let mut json = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut json)?;
    anyhow::ensure!(
        json.len() <= 16 * 1024 * 1024,
        "{} exceeds the 16 MiB state-file limit",
        path.display()
    );
    parse_user_templates(&json).with_context(|| format!("Parsing {}", path.display()))
}

pub fn parse_user_templates(json: &[u8]) -> Result<Vec<LcdTemplate>> {
    let file: TemplateFile = serde_json::from_slice(json)?;
    Ok(file.templates)
}

pub fn save_user_templates(path: &Path, templates: &[LcdTemplate]) -> Result<()> {
    let file = TemplateFile {
        templates: templates.to_vec(),
    };
    crate::persistence::write_json(path, &file)
}

pub fn install_user_template(path: &Path, template: &LcdTemplate) -> Result<()> {
    crate::persistence::update_json(path, |previous| {
        let mut templates = previous
            .map(parse_user_templates)
            .transpose()?
            .unwrap_or_default();
        if let Some(slot) = templates
            .iter_mut()
            .find(|existing| existing.id == template.id)
        {
            *slot = template.clone();
        } else {
            templates.push(template.clone());
        }
        Ok(TemplateFile { templates })
    })
}

pub fn all_templates(user: &[LcdTemplate], _sensors: &[SensorInfo]) -> Vec<LcdTemplate> {
    user.to_vec()
}

pub fn merge_user_templates(
    path: &Path,
    originals: &[LcdTemplate],
    copies: &[LcdTemplate],
) -> Result<()> {
    anyhow::ensure!(
        originals.len() <= 1024 && copies.len() <= 1024,
        "Too many templates in copied selection"
    );
    let mut ids = std::collections::HashSet::new();
    anyhow::ensure!(
        copies.iter().all(|template| ids.insert(&template.id)),
        "Copied template IDs must be unique"
    );
    crate::persistence::update_json(path, |previous| {
        let mut templates = previous
            .map(parse_user_templates)
            .transpose()?
            .unwrap_or_default();
        for copy in copies {
            let current = templates.iter().position(|template| template.id == copy.id);
            let original = originals.iter().find(|template| template.id == copy.id);
            anyhow::ensure!(
                serde_json::to_value(current.map(|index| &templates[index]))?
                    == serde_json::to_value(original)?,
                "Template '{}' changed. Reload and review it before saving copied paths.",
                copy.id
            );
            if let Some(index) = current {
                templates[index] = copy.clone();
            } else {
                templates.push(copy.clone());
            }
        }
        Ok(TemplateFile { templates })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_template_merge_preserves_unrelated_edits_and_rejects_stale_originals() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("templates.json");
        let original: LcdTemplate = serde_json::from_str(include_str!(
            "../../../templates/assets/cooler/template.json"
        ))
        .unwrap();
        let mut other = original.clone();
        other.id = "other".into();
        save_user_templates(&path, &[original.clone(), other.clone()]).unwrap();
        other.name = "Concurrent unrelated edit".into();
        install_user_template(&path, &other).unwrap();
        let mut copied = original.clone();
        copied.name = "Copied".into();
        merge_user_templates(
            &path,
            std::slice::from_ref(&original),
            std::slice::from_ref(&copied),
        )
        .unwrap();
        let updated = read_user_templates(&path).unwrap();
        assert_eq!(updated[0].name, "Copied");
        assert_eq!(updated[1].name, "Concurrent unrelated edit");
        let before = fs::read(&path).unwrap();
        let backup = fs::read(crate::persistence::backup_path(&path)).unwrap();
        assert!(merge_user_templates(&path, &[original], &[copied]).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::read(crate::persistence::backup_path(&path)).unwrap(),
            backup
        );
    }

    #[test]
    fn catalog_upsert_preserves_other_templates_and_rejects_invalid_collections() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("templates.json");
        let mut first: LcdTemplate = serde_json::from_str(include_str!(
            "../../../templates/assets/cooler/template.json"
        ))
        .unwrap();
        install_user_template(&path, &first).unwrap();
        let mut second = first.clone();
        second.id = "second".into();
        install_user_template(&path, &second).unwrap();
        first.name = "Updated".into();
        install_user_template(&path, &first).unwrap();
        let templates = read_user_templates(&path).unwrap();
        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].name, "Updated");
        assert_eq!(templates[1].id, "second");
        let backup = fs::read(crate::persistence::backup_path(&path)).unwrap();
        let invalid = br#"{"templates":"invalid collection"}"#;
        fs::write(&path, invalid).unwrap();
        assert!(install_user_template(&path, &first).is_err());
        assert_eq!(fs::read(&path).unwrap(), invalid);
        assert_eq!(
            fs::read(crate::persistence::backup_path(&path)).unwrap(),
            backup
        );
    }

    #[test]
    fn missing_templates_are_empty_but_invalid_or_nonregular_state_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("templates.json");
        assert!(read_user_templates(&path).unwrap().is_empty());
        fs::write(&path, b"{").unwrap();
        assert!(read_user_templates(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{");
        fs::write(&path, br#"{"templates":"damaged"}"#).unwrap();
        assert!(read_user_templates(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), br#"{"templates":"damaged"}"#);
        fs::write(&path, br#"{"templates":[]}"#).unwrap();
        assert!(read_user_templates(&path).unwrap().is_empty());
        assert!(read_user_templates(root.path()).is_err());
        fs::File::create(&path)
            .unwrap()
            .set_len(16 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_user_templates(&path)
            .unwrap_err()
            .to_string()
            .contains("16 MiB"));
    }
}
