#[path = "build/frontend.rs"]
mod frontend;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    for input in frontend::INPUTS.iter().copied().chain(["public", "dist"]) {
        println!("cargo:rerun-if-changed={}", root.join(input).display());
    }
    if let Err(error) = frontend::verify(&root) {
        let npm = find_npm().unwrap_or_else(|| {
            panic!("Frontend is not verified: {error}. Install npm and rebuild, or supply a frontend built from these sources with npm ci and npm run build.")
        });
        run(&npm, &["ci", "--no-audit", "--no-fund"], &root);
        run(&npm, &["run", "build"], &root);
        frontend::verify(&root).expect("npm build did not produce a matching frontend manifest");
    }
    tauri_build::build();
}

fn find_npm() -> Option<PathBuf> {
    if Command::new("npm")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Some(PathBuf::from("npm"));
    }
    let output = Command::new("sh")
        .args(["-lc", "command -v npm"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn run(command: &Path, args: &[&str], directory: &Path) {
    let status = Command::new(command)
        .args(args)
        .current_dir(directory)
        .status()
        .unwrap_or_else(|error| panic!("Failed to invoke {}: {error}", command.display()));
    assert!(
        status.success(),
        "{} {} exited with {status}",
        command.display(),
        args.join(" ")
    );
}
