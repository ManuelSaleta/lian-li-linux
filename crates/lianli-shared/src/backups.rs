use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupTarget {
    Configuration,
    Templates,
    RgbPresets,
    Profile { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub target: BackupTarget,
    #[serde(default)]
    pub preserved: bool,
    pub bytes: u64,
    pub modified_unix_seconds: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupPreview {
    pub target: BackupTarget,
    #[serde(default)]
    pub preserved: bool,
    pub sha256: String,
    pub bytes: usize,
    pub json: String,
    pub truncated: bool,
    pub parse_error: Option<String>,
    #[serde(default)]
    pub validation_error: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}
