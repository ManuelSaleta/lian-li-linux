//! Build script: runs Tauri's codegen, then builds the Vue frontend with npm
//! so a plain `cargo build` (or `cargo build -p lianli-gui-vue`) produces a
//! runnable GUI without a separate frontend step.
//!
//! Behaviour:
//! - Falls back gracefully if `npm` is not on PATH: prints a `cargo:warning`
//!   and leaves the existing `dist/` (or the checked-in placeholder) in place.
//! - Only re-runs `npm install` / `npm run build` when the frontend sources are
//!   newer than the built `dist/`, so Rust-only edits don't pay the vite cost.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    // Tauri config validation + codegen (emits its own rerun-if-changed).
    tauri_build::build();

    let frontend = frontend_dir();
    declare_inputs(&frontend);

    let npm = find_npm();
    if npm.is_none() {
        if !frontend.join("dist/index.html").exists() {
            println!(
                "cargo:warning=npm not found on PATH and dist/ is missing; \
                 install node/npm or run the frontend build manually"
            );
        } else {
            println!("cargo:warning=npm not found on PATH; using existing dist/ as-is");
        }
        return;
    }

    if needs_build(&frontend) {
        let npm_str = npm
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "npm".to_string());
        if !frontend.join("node_modules").exists() {
            run(&npm_str, &["install", "--no-audit", "--no-fund"], &frontend);
        }
        run(&npm_str, &["run", "build"], &frontend);
    }
}

/// Directory containing `package.json` (the crate root, one level up from
/// `src-tauri/`).
fn frontend_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..")
}

/// Tell cargo which inputs should re-trigger this script.
fn declare_inputs(frontend: &Path) {
    for f in [
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "tsconfig.json",
        "index.html",
    ] {
        println!("cargo:rerun-if-changed={}/{}", frontend.display(), f);
    }
    println!("cargo:rerun-if-changed={}/src", frontend.display());
}

/// Locate the `npm` executable. Checks PATH directly, then falls back to the
/// login shell's resolved PATH (which sources `.bashrc`/`.zshrc`).
fn find_npm() -> Option<PathBuf> {
    use std::path::Path;

    // 1. Direct PATH check — works when cargo inherits the right env.
    if Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Some(PathBuf::from("npm"));
    }

    // 2. Login-shell PATH (sources .bashrc / .zshrc / .profile).
    let out = Command::new("sh")
        .arg("-lc")
        .arg("command -v npm")
        .output()
        .ok()?;
    if out.status.success() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() && Path::new(&path).exists() {
            return Some(PathBuf::from(path));
        }
    }

    None
}

/// Rebuild iff any tracked frontend input is newer than the newest `dist/`
/// artifact (or `dist/` / `node_modules` is absent).
fn needs_build(frontend: &Path) -> bool {
    if !frontend.join("node_modules").exists() {
        return true;
    }
    let dist = frontend.join("dist");
    let newest_dist = newest_under(&dist);
    if newest_dist == SystemTime::UNIX_EPOCH {
        return true;
    }
    let mut newest_src = SystemTime::UNIX_EPOCH;
    for f in [
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "tsconfig.json",
        "index.html",
    ] {
        newest_src = newest_src.max(mtime(&frontend.join(f)));
    }
    newest_src = newest_src.max(newest_under(&frontend.join("src")));
    newest_src > newest_dist
}

fn run(cmd: &str, args: &[&str], dir: &Path) {
    let status = Command::new(cmd).args(args).current_dir(dir).status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("`{cmd} {}` exited with {s}", args.join(" ")),
        Err(e) => panic!("failed to invoke `{cmd}`: {e}"),
    }
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Newest modified time among the files in `dir` (recursive).
fn newest_under(dir: &Path) -> SystemTime {
    let mut best = SystemTime::UNIX_EPOCH;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return best;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            best = best.max(newest_under(&path));
        } else if ft.is_file() {
            best = best.max(mtime(&path));
        }
    }
    best
}
