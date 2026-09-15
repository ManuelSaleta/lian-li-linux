use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DAEMON_LOCK_PATH: &str = "/run/lianli-daemon.lock";

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
        let host_visible = Path::new("/run/host/run").is_dir();
        let container = box_name.is_some()
            || std::env::var_os("container").is_some()
            || Path::new("/run/.containerenv").exists()
            || Path::new("/.dockerenv").exists()
            || Path::new("/run/systemd/container").exists()
            || Path::new("/run/host").exists();
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
