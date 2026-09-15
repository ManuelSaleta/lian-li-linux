use lianli_shared::daemon::{DaemonBuildInfo, IPC_PROTOCOL_VERSION, SERVICE_SELECTION};

#[test]
fn capabilities_work_when_service_startup_is_forbidden_and_configuration_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("missing.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lianli-daemon"))
        .env_remove("CONTAINER_ID")
        // Unsupported containers fail before ownership if this regresses into service startup.
        .env("container", "capability-test")
        .arg("--config")
        .arg(&config)
        .arg("--socket")
        .arg(root.path().join("socket"))
        .arg("capabilities")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let build: DaemonBuildInfo = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(build.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(build.protocol_version, IPC_PROTOCOL_VERSION);
    assert!(build
        .capabilities
        .iter()
        .any(|value| value == SERVICE_SELECTION));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
