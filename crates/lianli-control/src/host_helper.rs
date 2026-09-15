use anyhow::{ensure, Context, Result};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const PACKAGED: &str = "/usr/bin/lianli-control";
pub const STANDALONE: &str = "/usr/local/libexec/lianli/lianli-control";

pub fn installed() -> Result<PathBuf> {
    for candidate in [PACKAGED, STANDALONE] {
        match fs::symlink_metadata(candidate) {
            Ok(_) => return verify_path(Path::new(candidate), Path::new("/"), 0),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect installed host helper"),
        }
    }
    anyhow::bail!("Install the native control helper or the standalone Distrobox host helper")
}

fn verify_path(path: &Path, boundary: &Path, owner: u32) -> Result<PathBuf> {
    fn chain(path: &Path, boundary: &Path, owner: u32) -> Result<()> {
        ensure!(
            path.starts_with(boundary),
            "Host helper left its trusted directory"
        );
        for current in path
            .ancestors()
            .take_while(|value| value.starts_with(boundary))
        {
            let metadata = fs::symlink_metadata(current)?;
            ensure!(
                metadata.uid() == owner && (metadata.is_symlink() || metadata.mode() & 0o022 == 0),
                "Host helper paths must be root-owned and not writable by other accounts"
            );
            if current != path {
                ensure!(
                    metadata.is_dir() || metadata.is_symlink(),
                    "Host helper parent is not a directory"
                );
            }
        }
        Ok(())
    }
    chain(path, boundary, owner)?;
    let resolved = fs::canonicalize(path)?;
    let boundary = fs::canonicalize(boundary)?;
    chain(&resolved, &boundary, owner)?;
    let metadata = fs::symlink_metadata(&resolved)?;
    ensure!(
        metadata.is_file()
            && metadata.mode() & 0o111 != 0
            && resolved.file_name() == Some(std::ffi::OsStr::new("lianli-control")),
        "The installed host control helper is not executable"
    );
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn accepts_protected_symlink_layouts_but_rejects_writable_targets_and_parents() {
        let root = tempfile::tempdir().unwrap();
        let owner = unsafe { libc::geteuid() };
        let real = root.path().join("var/usrlocal/libexec/lianli");
        fs::create_dir_all(&real).unwrap();
        let binary = real.join("lianli-control");
        fs::write(&binary, b"fixture").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(root.path().join("var/usrlocal"), root.path().join("local")).unwrap();
        let linked = root.path().join("local/libexec/lianli/lianli-control");
        assert_eq!(verify_path(&linked, root.path(), owner).unwrap(), binary);
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(verify_path(&linked, root.path(), owner).is_err());
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify_path(&linked, root.path(), owner).is_err());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(verify_path(&linked, root.path(), owner).is_err());
        assert!(verify_path(&linked, root.path(), owner.wrapping_add(1)).is_err());
    }
}
