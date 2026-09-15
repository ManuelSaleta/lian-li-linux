use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const IPC_PROTOCOL_VERSION: u32 = 1;
pub const GUARDED_WRITES: &str = "guarded_writes";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteGuard {
    pub client_version: String,
    pub protocol_version: u32,
    pub instance_id: String,
}

impl DaemonInfo {
    pub fn write_guard(&self, client_version: &str) -> Result<WriteGuard, String> {
        if self.version != client_version || self.protocol_version != IPC_PROTOCOL_VERSION {
            return Err(format!(
                "Changes are disabled: GUI/client {client_version} and daemon {} must use matching versions. Update both and restart the selected daemon cleanly.",
                self.version
            ));
        }
        if !self
            .capabilities
            .iter()
            .any(|capability| capability == GUARDED_WRITES)
        {
            return Err("Changes are disabled: this daemon lacks compatibility checks. Update both applications and restart the selected daemon cleanly.".into());
        }
        Ok(WriteGuard {
            client_version: client_version.into(),
            protocol_version: IPC_PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
        })
    }
}

impl WriteGuard {
    pub fn validate(&self, daemon: &DaemonInfo) -> Result<(), String> {
        let expected = daemon.write_guard(&self.client_version)?;
        if self != &expected {
            return Err("The daemon changed or the IPC version is incompatible. Refresh before applying changes.".into());
        }
        Ok(())
    }
}

/// Configuration scope selected at launch; this does not imply systemd ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    User,
    System,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub protocol_version: u32,
    pub instance_id: String,
    pub pid: u32,
    pub mode: DaemonMode,
    pub config_path: PathBuf,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::IpcRequest;
    use serde_json::json;

    fn current() -> DaemonInfo {
        DaemonInfo {
            version: "1.0.0".into(),
            protocol_version: IPC_PROTOCOL_VERSION,
            instance_id: "instance-a".into(),
            pid: 123,
            mode: DaemonMode::User,
            config_path: "/example/config.json".into(),
            capabilities: vec![GUARDED_WRITES.into()],
        }
    }

    #[test]
    fn writes_require_matching_versions_capabilities_and_instance() {
        let daemon = current();
        let guard = daemon.write_guard("1.0.0").unwrap();
        guard.validate(&daemon).unwrap();
        assert!(daemon.write_guard("0.9.1").is_err());
        let mut changed = daemon.clone();
        changed.instance_id = "instance-b".into();
        assert!(guard.validate(&changed).is_err());
        changed = daemon.clone();
        changed.protocol_version += 1;
        assert!(guard.validate(&changed).is_err());
        changed = daemon.clone();
        changed.capabilities.clear();
        assert!(guard.validate(&changed).is_err());
    }

    #[test]
    fn legacy_reads_survive_but_writes_and_nested_guards_are_rejected() {
        let daemon = current();
        assert!(IpcRequest::GetConfig.authorize(&daemon).is_ok());
        let write = IpcRequest::SetLcdTemplates { templates: vec![] };
        assert!(write.clone().authorize(&daemon).is_err());
        let guarded = IpcRequest::Guarded {
            guard: daemon.write_guard("1.0.0").unwrap(),
            request: Box::new(write),
        };
        let wire = serde_json::to_value(&guarded).unwrap();
        let decoded: IpcRequest = serde_json::from_value(wire).unwrap();
        assert!(matches!(
            decoded.authorize(&daemon).unwrap(),
            IpcRequest::SetLcdTemplates { .. }
        ));
        let nested = IpcRequest::Guarded {
            guard: daemon.write_guard("1.0.0").unwrap(),
            request: Box::new(guarded),
        };
        assert!(nested.authorize(&daemon).is_err());
    }

    #[test]
    fn older_identity_payloads_can_omit_capabilities() {
        let info: DaemonInfo = serde_json::from_value(json!({
            "version": "1.0.0", "protocol_version": 1, "instance_id": "a",
            "pid": 123, "mode": "system", "config_path": "/var/lib/lianli/config.json"
        }))
        .unwrap();
        assert_eq!(info.mode, DaemonMode::System);
        assert!(info.capabilities.is_empty());
    }

    #[test]
    fn future_modes_and_capabilities_do_not_break_identity_parsing() {
        let info: DaemonInfo = serde_json::from_value(json!({
            "version": "2.0.0", "protocol_version": 2, "instance_id": "b",
            "pid": 456, "mode": "future", "config_path": "/example/config.json",
            "capabilities": ["future_feature"], "future_field": true
        }))
        .unwrap();
        assert_eq!(info.mode, DaemonMode::Unknown);
        assert_eq!(info.capabilities, ["future_feature"]);
    }

    #[test]
    fn identity_request_preserves_existing_ping_wire_format() {
        assert_eq!(
            serde_json::to_value(IpcRequest::Ping).unwrap(),
            json!({"method": "Ping"})
        );
        let request: IpcRequest = serde_json::from_value(json!({
            "method": "GetDaemonInfo", "params": null
        }))
        .unwrap();
        assert!(matches!(request, IpcRequest::GetDaemonInfo));
    }
}
