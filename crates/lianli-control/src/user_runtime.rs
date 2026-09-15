use anyhow::{ensure, Result};
use lianli_shared::daemon::FileIdentity;
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(crate) fn identity(uid: u32) -> Result<Option<FileIdentity>> {
    ensure!(uid != 0, "A desktop user is required for runtime identity");
    identity_at(Path::new(&format!("/run/user/{uid}")), uid)
}

fn identity_at(path: &Path, uid: u32) -> Result<Option<FileIdentity>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "The user runtime directory must be private and owned by its account"
    );
    Ok(Some(FileIdentity {
        device: metadata.dev().to_string(),
        inode: metadata.ino().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    #[test]
    fn runtime_identity_detects_logout_recreation_and_rejects_unsafe_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime");
        let uid = unsafe { libc::geteuid() };
        assert!(identity_at(&path, uid).unwrap().is_none());
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let original = identity_at(&path, uid).unwrap().unwrap();
        fs::write(path.join("bus-fixture"), b"").unwrap();
        assert_eq!(identity_at(&path, uid).unwrap().unwrap(), original);
        fs::rename(&path, temporary.path().join("previous")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        assert_ne!(identity_at(&path, uid).unwrap().unwrap(), original);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(identity_at(&path, uid).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(identity_at(&path, uid.saturating_add(1)).is_err());
        fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(temporary.path().join("previous"), &path).unwrap();
        assert!(identity_at(&path, uid).is_err());
    }
}
