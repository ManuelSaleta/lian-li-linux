#[path = "../build/frontend.rs"]
mod frontend;

use std::fs;
use std::path::Path;
use std::process::Command;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for input in frontend::INPUTS {
        if ["src", "scripts"].contains(input) {
            fs::create_dir(root.path().join(input)).unwrap();
        } else {
            fs::write(root.path().join(input), b"input").unwrap();
        }
    }
    fs::write(root.path().join("src/main.ts"), b"source").unwrap();
    fs::write(
        root.path().join("scripts/stamp-build.mjs"),
        include_str!("../../scripts/stamp-build.mjs"),
    )
    .unwrap();
    fs::create_dir(root.path().join("dist")).unwrap();
    fs::write(
        root.path().join("dist/index.html"),
        b"<script src='app.js'></script>",
    )
    .unwrap();
    fs::write(root.path().join("dist/app.js"), b"compiled").unwrap();
    root
}

fn stamp(root: &Path) {
    let output = Command::new("node")
        .arg("scripts/stamp-build.mjs")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn source_changes_and_new_files_invalidate_prebuilt_frontend() {
    let root = fixture();
    assert!(frontend::verify(root.path()).is_err());
    stamp(root.path());
    assert!(frontend::verify(root.path()).is_ok());
    fs::write(root.path().join("src/main.ts"), b"changed").unwrap();
    assert!(frontend::verify(root.path()).is_err());
    stamp(root.path());
    fs::write(root.path().join("src/added.ts"), b"new").unwrap();
    assert!(frontend::verify(root.path()).is_err());
}

#[test]
fn modified_or_missing_bundles_cannot_pass_as_a_built_frontend() {
    let root = fixture();
    stamp(root.path());
    fs::write(root.path().join("dist/app.js"), b"stale bundle").unwrap();
    assert!(frontend::verify(root.path()).is_err());
    stamp(root.path());
    fs::remove_file(root.path().join("dist/app.js")).unwrap();
    assert!(frontend::verify(root.path()).is_err());
}
