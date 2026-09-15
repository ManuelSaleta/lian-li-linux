use super::EventSender;

use lianli_shared::ipc::IpcResponse;
use lianli_shared::profile::DeviceProfile;
use tracing::info;

use crate::ipc::{DaemonState, SharedState};
use crate::persistence::write_json;
use crate::service::DaemonEvent;

fn profiles_dir(state: &DaemonState) -> std::path::PathBuf {
    state
        .config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("profiles")
}

fn profile_path(state: &DaemonState, name: &str) -> Result<std::path::PathBuf, &'static str> {
    if name.is_empty()
        || name.len() > 250
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
    {
        return Err("Profile name must be a single filename, without path separators, and at most 250 bytes");
    }
    Ok(profiles_dir(state).join(format!("{name}.json")))
}

fn read_all_profiles(state: &DaemonState) -> Vec<DeviceProfile> {
    let dir = profiles_dir(state);
    let mut profiles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(p) = serde_json::from_str::<DeviceProfile>(&data) {
                        profiles.push(p);
                    }
                }
            }
        }
    }
    profiles
}

pub fn save(state: &SharedState, tx: EventSender, name: String, device_id: String) -> IpcResponse {
    let st = state.lock();
    let device = st.devices.iter().find(|d| d.device_id == device_id);
    let family = device
        .map(|d| {
            serde_json::to_string(&d.family)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string()
        })
        .unwrap_or_default();
    let config = match st.config.as_ref() {
        Some(c) => c,
        None => return IpcResponse::error("no config loaded"),
    };
    let profile = DeviceProfile::capture_from_config(config, &name, &device_id, &family);
    let path = match profile_path(&st, &name) {
        Ok(path) => path,
        Err(error) => return IpcResponse::error(error),
    };
    if let Err(e) = write_json(&path, &profile) {
        return IpcResponse::error(format!("failed to write profile: {e}"));
    }
    drop(st);
    let _ = tx.send(DaemonEvent::IpcUpdate);
    info!("Device profile '{name}' saved for {device_id} ({family})");
    IpcResponse::ok(serde_json::json!(null))
}

pub fn delete(state: &SharedState, tx: EventSender, name: String) -> IpcResponse {
    let st = state.lock();
    let path = match profile_path(&st, &name) {
        Ok(path) => path,
        Err(error) => return IpcResponse::error(error),
    };
    if !path.exists() {
        return IpcResponse::error(format!("profile '{name}' not found"));
    }
    if let Err(e) = std::fs::remove_file(&path) {
        return IpcResponse::error(format!("failed to delete profile: {e}"));
    }
    drop(st);
    let _ = tx.send(DaemonEvent::IpcUpdate);
    info!("Device profile '{name}' deleted");
    IpcResponse::ok(serde_json::json!(null))
}

pub fn list(state: &SharedState) -> IpcResponse {
    let st = state.lock();
    let profiles = read_all_profiles(&st);
    let entries: Vec<serde_json::Value> = profiles
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "device_id": p.device_id,
                "device_family": p.device_family
            })
        })
        .collect();
    IpcResponse::ok(entries)
}

pub fn apply(state: &SharedState, tx: EventSender, name: String, device_id: String) -> IpcResponse {
    let mut st = state.lock();
    let profile = {
        let profiles = read_all_profiles(&st);
        profiles.into_iter().find(|p| p.name == name)
    };
    let Some(mut profile) = profile else {
        return IpcResponse::error(format!("profile '{name}' not found"));
    };
    profile.device_id = device_id.clone();
    let mut config = st.config.clone().unwrap_or_default();
    profile.apply_to_config(&mut config);
    super::persist_and_notify(&mut st, &tx, "ApplyProfile", config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::config::AppConfig;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[test]
    fn profile_names_cannot_overwrite_or_delete_the_main_configuration() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let config = AppConfig::default();
        crate::persistence::write_config(&path, &config).unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut daemon = DaemonState::new(path.clone());
        daemon.config = Some(config);
        let state = Arc::new(Mutex::new(daemon));
        let (tx, _) = std::sync::mpsc::channel();
        for name in ["../config", "/tmp/config", "..\\config", "", ".", ".."] {
            assert!(matches!(
                save(&state, tx.clone().into(), name.into(), "test".into()),
                IpcResponse::Error { .. }
            ));
            assert!(matches!(
                delete(&state, tx.clone().into(), name.into()),
                IpcResponse::Error { .. }
            ));
        }
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert!(profile_path(&state.lock(), "Quiet gaming")
            .unwrap()
            .ends_with("profiles/Quiet gaming.json"));
    }
}
