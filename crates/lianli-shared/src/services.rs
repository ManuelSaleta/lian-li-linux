use crate::installation::InstallationContext;
use serde::{Deserialize, Serialize};

pub const USER_UNIT: &str = "lianli-daemon.service";
pub const SYSTEM_UNIT: &str = "lianli-daemon-system.service";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceProbe<T> {
    Known { value: T },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitState {
    pub name: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub unit_file_state: String,
    pub main_pid: u32,
    pub fragment_path: String,
    #[serde(default)]
    pub control_group: Option<String>,
    #[serde(default)]
    pub kill_mode: Option<String>,
    #[serde(default)]
    pub send_sigkill: Option<bool>,
    #[serde(default)]
    pub graceful_shutdown: Option<bool>,
    #[serde(default)]
    pub invocation_id: Option<String>,
    #[serde(default)]
    pub distrobox_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceReport {
    pub context: InstallationContext,
    pub user: ServiceProbe<UnitState>,
    pub system: ServiceProbe<UnitState>,
    pub global_user: ServiceProbe<String>,
    #[serde(default)]
    pub ownership: Option<ServiceProbe<OwnershipSnapshot>>,
    #[serde(default)]
    pub operation_lock: Option<ServiceProbe<crate::daemon::FileIdentity>>,
    #[serde(default)]
    pub selection: Option<ServiceProbe<Option<ServiceSelection>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSelection {
    pub scope: ServiceScope,
    pub uid: u32,
}

impl ServiceSelection {
    pub fn allows(self, scope: ServiceScope, uid: u32) -> bool {
        self.scope == scope && self.uid == uid
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnershipSnapshot {
    pub identity: crate::daemon::FileIdentity,
    pub owner_pid: Option<u32>,
    #[serde(default)]
    pub process: Option<ServiceProbe<OwnerProcess>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceScope {
    User,
    System,
}

impl ServiceScope {
    pub fn unit(self) -> &'static str {
        match self {
            Self::User => USER_UNIT,
            Self::System => SYSTEM_UNIT,
        }
    }

    pub fn argument(self) -> &'static str {
        match self {
            Self::User => "--user",
            Self::System => "--system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl ServiceAction {
    pub fn argument(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ServiceActionRequest {
    pub scope: ServiceScope,
    pub action: ServiceAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceChangeRequest {
    Switch {
        scope: ServiceScope,
        #[serde(default)]
        carry_settings: bool,
    },
    Recover {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceOperationStatus {
    pub active: bool,
    pub message: String,
    pub success: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerProcess {
    pub pid: u32,
    pub effective_uid: u32,
    pub start_time_ticks: String,
    pub control_group: String,
    pub service: Option<ServiceScope>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_changes_preserve_destination_settings_unless_copying_is_requested() {
        let request: ServiceChangeRequest =
            serde_json::from_str(r#"{"kind":"switch","scope":"system"}"#).unwrap();
        assert_eq!(
            request,
            ServiceChangeRequest::Switch {
                scope: ServiceScope::System,
                carry_settings: false
            }
        );
        assert_eq!(
            serde_json::from_str::<ServiceChangeRequest>(r#"{"kind":"recover"}"#).unwrap(),
            ServiceChangeRequest::Recover {}
        );
        assert!(serde_json::from_str::<ServiceChangeRequest>(
            r#"{"kind":"recover","scope":"user"}"#
        )
        .is_err());
    }

    #[test]
    fn selected_mode_and_account_must_both_match() {
        let selected = ServiceSelection {
            scope: ServiceScope::User,
            uid: 1000,
        };
        assert!(selected.allows(ServiceScope::User, 1000));
        assert!(!selected.allows(ServiceScope::User, 1001));
        assert!(!selected.allows(ServiceScope::System, 1000));
        assert!(!selected.allows(ServiceScope::System, 0));
        let old: ServiceReport = serde_json::from_value(serde_json::json!({
            "context":{"kind":"native"},
            "user":{"state":"unavailable","reason":"offline"},
            "system":{"state":"unavailable","reason":"offline"},
            "global_user":{"state":"known","value":"disabled"}
        }))
        .unwrap();
        assert!(old.selection.is_none());
    }

    #[test]
    fn old_service_snapshots_leave_process_and_cgroup_identity_unverified() {
        let unit: UnitState = serde_json::from_value(serde_json::json!({
            "name": USER_UNIT, "load_state": "loaded", "active_state": "active",
            "sub_state": "running", "unit_file_state": "enabled", "main_pid": 123,
            "fragment_path": "/usr/lib/systemd/user/lianli-daemon.service"
        }))
        .unwrap();
        assert!(unit.control_group.is_none());
        assert!(unit.invocation_id.is_none());
        assert!(unit.distrobox_name.is_none());
        let owner: OwnershipSnapshot = serde_json::from_value(serde_json::json!({
            "identity": { "device": "47", "inode": "9007199254740993" },
            "owner_pid": 123
        }))
        .unwrap();
        assert!(owner.process.is_none());
        assert_eq!(owner.identity.inode, "9007199254740993");
    }
}
