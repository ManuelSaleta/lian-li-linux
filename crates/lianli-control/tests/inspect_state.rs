use std::process::Command;

#[test]
fn runtime_diagnostics_preserve_the_calling_account_and_reject_hidden_host_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_lianli-control"))
        .arg("diagnose-runtime")
        .env("container", "lianli-private-test")
        .env_remove("CONTAINER_ID")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: lianli_control::runtime_health::Report =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.uid, unsafe { libc::geteuid() });
    assert_eq!(report.gid, unsafe { libc::getegid() });
    assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        report.hid_backend,
        Some(lianli_shared::config::HidBackend::Hidraw)
    );
    assert_eq!(report.findings.len(), 2);
    assert_eq!(report.findings[1].code, "usb.node_access");
    assert_eq!(
        report.findings[1].state,
        lianli_shared::installation::CheckState::Unavailable
    );
    assert_eq!(
        report.findings[0].state,
        lianli_shared::installation::CheckState::Unavailable
    );
    assert!(report.findings[0]
        .evidence
        .contains("host lock is unavailable"));
}

#[test]
fn automatic_recovery_rejects_an_unsupported_context_before_accessing_host_state() {
    let output = Command::new(env!("CARGO_BIN_EXE_lianli-control"))
        .arg("recover-automatic")
        .env("container", "lianli-private-test")
        .env_remove("CONTAINER_ID")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("requires the installed native system service"));
}

#[test]
fn asset_preflight_reports_failures_and_sets_exit_status_without_changing_saved_state() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.json");
    let raw = br#"{"lcds":[{"index":0,"type":"image","path":"missing.png"}],"future":true}"#;
    std::fs::write(&config, raw).unwrap();
    let inspect = |check_assets| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lianli-control"));
        command
            .arg("inspect-state")
            .arg("--config")
            .arg(&config)
            .arg("--working-directory")
            .arg(root.path());
        if check_assets {
            command.arg("--check-assets");
        }
        command.output().unwrap()
    };
    assert!(inspect(false).status.success());
    let failed = inspect(true);
    assert!(!failed.status.success());
    let report: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(report["assets"]["checked"], 1);
    assert_eq!(report["assets"]["failed"], 1);
    assert!(report["assets"]["issues"][0]["owner"]
        .as_str()
        .unwrap()
        .contains("LCD[index:0]"));
    assert_eq!(std::fs::read(&config).unwrap(), raw);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    std::fs::write(root.path().join("missing.png"), b"readable, not decoded").unwrap();
    let passed = inspect(true);
    assert!(passed.status.success());
    let report: serde_json::Value = serde_json::from_slice(&passed.stdout).unwrap();
    assert_eq!(report["assets"]["failed"], 0);
    assert_eq!(std::fs::read(&config).unwrap(), raw);
}
