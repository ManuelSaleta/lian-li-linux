use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DAEMON_LOCK_PATH: &str = "/run/lianli-daemon.lock";
pub const SERVICE_OPERATION_LOCK_PATH: &str = "/run/lianli-control.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Passed,
    Failed,
    Unavailable,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationGuide {
    UsbPermissions,
    ServiceModes,
    Distrobox,
    Troubleshooting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationFinding {
    pub code: String,
    pub state: CheckState,
    pub severity: FindingSeverity,
    pub feature: String,
    pub context: String,
    pub title: String,
    pub evidence: String,
    pub remediation: String,
    pub guide: InstallationGuide,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationReport {
    pub context: InstallationContext,
    #[serde(default)]
    pub daemon_context: Option<InstallationContext>,
    pub checked_at_unix_ms: u64,
    pub findings: Vec<InstallationFinding>,
    #[serde(default)]
    pub services: Option<crate::services::ServiceReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInstallationReport {
    pub instance_id: String,
    pub uid: u32,
    #[serde(default)]
    pub context: Option<InstallationContext>,
    pub findings: Vec<InstallationFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallationContext {
    Native,
    Distrobox { name: String },
    UnsupportedContainer,
}

impl InstallationContext {
    pub fn detect() -> Self {
        let box_name = std::env::var("CONTAINER_ID")
            .ok()
            .filter(|name| !name.is_empty());
        Self::detect_at(
            Path::new("/"),
            box_name,
            std::env::var_os("container").is_some(),
        )
    }

    fn detect_at(root: &Path, box_name: Option<String>, container_env: bool) -> Self {
        let host_visible = root.join("run/host/run").is_dir();
        let container = box_name.is_some()
            || container_env
            || root.join("run/.containerenv").exists()
            || root.join(".dockerenv").exists()
            || root.join("run/systemd/container").exists();
        Self::from_evidence(box_name, host_visible, container)
    }

    fn from_evidence(box_name: Option<String>, host_visible: bool, container: bool) -> Self {
        match (box_name, host_visible, container) {
            (Some(name), true, _) => Self::Distrobox { name },
            (_, _, true) => Self::UnsupportedContainer,
            _ => Self::Native,
        }
    }

    pub fn daemon_lock_path(&self) -> Option<PathBuf> {
        match self {
            Self::Native => Some(DAEMON_LOCK_PATH.into()),
            Self::Distrobox { .. } => Some(format!("/run/host{DAEMON_LOCK_PATH}").into()),
            Self::UnsupportedContainer => None,
        }
    }

    pub fn system_socket_path(&self) -> PathBuf {
        match self {
            Self::Distrobox { .. } => "/run/host/run/lianli/lianli-daemon.sock".into(),
            _ => "/run/lianli/lianli-daemon.sock".into(),
        }
    }

    pub fn service_operation_lock_path(&self) -> Option<PathBuf> {
        match self {
            Self::Native => Some(SERVICE_OPERATION_LOCK_PATH.into()),
            Self::Distrobox { .. } => {
                Some(format!("/run/host{SERVICE_OPERATION_LOCK_PATH}").into())
            }
            Self::UnsupportedContainer => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_toolbox_host_symlink_does_not_imply_a_container() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("run")).unwrap();
        std::os::unix::fs::symlink("../", root.path().join("run/host")).unwrap();
        assert!(root.path().join("run/host/run").is_dir());

        let context = InstallationContext::detect_at(root.path(), None, false);
        assert_eq!(context, InstallationContext::Native);
        assert_eq!(context.daemon_lock_path(), Some(DAEMON_LOCK_PATH.into()));
        assert_eq!(
            context.service_operation_lock_path(),
            Some(SERVICE_OPERATION_LOCK_PATH.into())
        );
    }

    #[test]
    fn container_markers_remain_unsupported_even_with_host_access() {
        for marker in ["run/.containerenv", ".dockerenv", "run/systemd/container"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("run/host/run")).unwrap();
            let path = root.path().join(marker);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();

            let context = InstallationContext::detect_at(root.path(), None, false);
            assert_eq!(
                context,
                InstallationContext::UnsupportedContainer,
                "{marker}"
            );
            assert_eq!(context.daemon_lock_path(), None);
            assert_eq!(
                InstallationContext::detect_at(root.path(), Some("box".into()), true),
                InstallationContext::Distrobox { name: "box".into() },
                "Distrobox with {marker} and the container environment variable"
            );
        }
    }

    #[test]
    fn environment_detection_still_requires_a_named_box_with_host_access() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            InstallationContext::detect_at(root.path(), None, false),
            InstallationContext::Native
        );
        assert_eq!(
            InstallationContext::detect_at(root.path(), None, true),
            InstallationContext::UnsupportedContainer
        );
        assert_eq!(
            InstallationContext::detect_at(root.path(), Some("box".into()), false),
            InstallationContext::UnsupportedContainer
        );
        std::fs::create_dir_all(root.path().join("run/host/run")).unwrap();
        assert_eq!(
            InstallationContext::detect_at(root.path(), None, false),
            InstallationContext::Native
        );
        assert_eq!(
            InstallationContext::detect_at(root.path(), None, true),
            InstallationContext::UnsupportedContainer
        );
        let context = InstallationContext::detect_at(root.path(), Some("box".into()), false);
        assert_eq!(
            context,
            InstallationContext::Distrobox { name: "box".into() }
        );
        assert_eq!(
            context.daemon_lock_path(),
            Some("/run/host/run/lianli-daemon.lock".into())
        );
    }

    #[test]
    fn daemon_environment_is_optional_for_old_reports_and_independent_of_gui_context() {
        let old: RuntimeInstallationReport = serde_json::from_value(
            serde_json::json!({"instance_id":"daemon", "uid":1000, "findings":[]}),
        )
        .unwrap();
        assert!(old.context.is_none());
        let old_gui: InstallationReport = serde_json::from_value(
            serde_json::json!({"context":{"kind":"native"}, "checked_at_unix_ms":1, "findings":[]}),
        )
        .unwrap();
        assert!(old_gui.daemon_context.is_none());
        let mixed: InstallationReport = serde_json::from_value(serde_json::json!({"context":{"kind":"native"}, "daemon_context":{"kind":"distrobox", "name":"fixture"}, "checked_at_unix_ms":1, "findings":[]})).unwrap();
        assert_eq!(mixed.context, InstallationContext::Native);
        assert_eq!(
            mixed.daemon_context,
            Some(InstallationContext::Distrobox {
                name: "fixture".into()
            })
        );
    }

    #[test]
    fn distrobox_uses_the_host_lock_instead_of_its_private_run_directory() {
        let context = InstallationContext::from_evidence(Some("a box".into()), true, true);
        assert_eq!(
            context,
            InstallationContext::Distrobox {
                name: "a box".into()
            }
        );
        assert_eq!(
            context.daemon_lock_path(),
            Some("/run/host/run/lianli-daemon.lock".into())
        );
        assert_eq!(
            InstallationContext::Native.daemon_lock_path(),
            Some(DAEMON_LOCK_PATH.into())
        );
        assert_eq!(
            context.system_socket_path(),
            PathBuf::from("/run/host/run/lianli/lianli-daemon.sock")
        );
        assert_eq!(
            InstallationContext::Native.system_socket_path(),
            PathBuf::from("/run/lianli/lianli-daemon.sock")
        );
    }

    #[test]
    fn unobservable_container_has_no_independent_lock_fallback() {
        for context in [
            InstallationContext::from_evidence(Some("box".into()), false, true),
            InstallationContext::from_evidence(None, false, true),
        ] {
            assert_eq!(context, InstallationContext::UnsupportedContainer);
            assert_eq!(context.daemon_lock_path(), None);
        }
    }
}
