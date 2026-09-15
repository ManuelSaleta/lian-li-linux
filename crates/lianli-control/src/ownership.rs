use crate::services::Route;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::FileIdentity;
use lianli_shared::installation::{InstallationContext, DAEMON_LOCK_PATH};
use lianli_shared::services::OwnershipSnapshot;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub fn inspect(context: &InstallationContext, route: &Route) -> Result<OwnershipSnapshot> {
    let path = context
        .daemon_lock_path()
        .context("Host ownership lock is unavailable")?;
    let identity = file_identity(&path, matches!(context, InstallationContext::Native))?;
    let table = match route {
        Route::Native => read_table(File::open("/proc/locks")?)?,
        Route::Host { .. } => {
            ensure!(
                host_identity(route)? == identity,
                "Container and host paths identify different ownership locks"
            );
            let output = route.output("/usr/bin/cat", &["/proc/locks"])?;
            ensure!(
                output.status.success(),
                "Cannot inspect host kernel locks: {}",
                output.stderr.trim()
            );
            let after = host_identity(route)?;
            ensure!(
                after == identity,
                "Host ownership lock changed during inspection"
            );
            output.stdout
        }
    };
    ensure!(
        file_identity(&path, matches!(context, InstallationContext::Native))? == identity,
        "Ownership lock changed during inspection"
    );
    let owner_pid = lock_owner(&table, &identity)?;
    Ok(OwnershipSnapshot {
        identity,
        owner_pid,
        process: None,
    })
}

pub(crate) fn file_identity(path: &Path, require_root: bool) -> Result<FileIdentity> {
    Ok(open_identity(path, require_root)?.1)
}

pub(crate) fn open_identity(path: &Path, require_root: bool) -> Result<(File, FileIdentity)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Ownership lock is not a regular file");
    ensure!(
        !require_root || metadata.uid() == 0,
        "Host ownership lock must belong to root"
    );
    Ok((
        file,
        FileIdentity {
            device: metadata.dev().to_string(),
            inode: metadata.ino().to_string(),
        },
    ))
}

pub(crate) fn host_identity(route: &Route) -> Result<FileIdentity> {
    host_identity_at(route, DAEMON_LOCK_PATH)
}

pub(crate) fn host_identity_at(route: &Route, path: &str) -> Result<FileIdentity> {
    let output = route.output("/usr/bin/stat", &["--printf=%d %i %f %u\n", "--", path])?;
    ensure!(
        output.status.success(),
        "Cannot inspect host ownership file: {}",
        output.stderr.trim()
    );
    parse_stat(&output.stdout)
}

fn parse_stat(text: &str) -> Result<FileIdentity> {
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure!(fields.len() == 4, "Invalid host lock metadata");
    let mode = u32::from_str_radix(fields[2], 16)?;
    ensure!(
        mode & libc::S_IFMT == libc::S_IFREG && fields[3] == "0",
        "Host ownership lock is not a root-owned regular file"
    );
    Ok(FileIdentity {
        device: fields[0].parse::<u64>()?.to_string(),
        inode: fields[1].parse::<u64>()?.to_string(),
    })
}

fn read_table(reader: impl Read) -> Result<String> {
    let mut content = String::new();
    reader.take(64 * 1024 + 1).read_to_string(&mut content)?;
    ensure!(
        content.len() <= 64 * 1024,
        "Kernel lock table exceeds the inspection limit"
    );
    Ok(content)
}

fn lock_owner(table: &str, identity: &FileIdentity) -> Result<Option<u32>> {
    let device = identity.device.parse::<u64>()?;
    let inode = identity.inode.parse::<u64>()?;
    let mut owner = None;
    for line in table.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(1) == Some(&"->") {
            continue;
        }
        ensure!(fields.len() == 8, "Unrecognized kernel lock record");
        let key: Vec<_> = fields[5].split(':').collect();
        ensure!(key.len() == 3, "Invalid kernel lock file identity");
        let major = u32::from_str_radix(key[0], 16)?;
        let minor = u32::from_str_radix(key[1], 16)?;
        if libc::makedev(major, minor) != device || key[2].parse::<u64>()? != inode {
            continue;
        }
        ensure!(
            fields[1] == "FLOCK"
                && fields[2] == "ADVISORY"
                && fields[3] == "WRITE"
                && fields[6] == "0"
                && fields[7] == "EOF",
            "Unexpected lock type on the daemon ownership file"
        );
        let pid = fields[4]
            .parse::<u32>()
            .context("Kernel lock owner is not identifiable")?;
        ensure!(
            pid > 0 && owner.is_none(),
            "Ambiguous kernel lock ownership"
        );
        owner = Some(pid);
    }
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> FileIdentity {
        FileIdentity {
            device: libc::makedev(0, 47).to_string(),
            inode: "9007199254740993".into(),
        }
    }

    #[test]
    fn matches_full_file_identity_and_ignores_waiting_contenders() {
        let table = "1: FLOCK ADVISORY WRITE 123 00:2f:9007199254740993 0 EOF\n2: -> FLOCK ADVISORY WRITE 456 00:2f:9007199254740993 0 EOF\n3: FLOCK ADVISORY WRITE 789 08:2f:9007199254740993 0 EOF\n";
        assert_eq!(lock_owner(table, &identity()).unwrap(), Some(123));
        assert_eq!(lock_owner("", &identity()).unwrap(), None);
        assert_eq!(
            lock_owner(
                "1: FLOCK ADVISORY WRITE 123 00:2f:9007199254740992 0 EOF",
                &identity()
            )
            .unwrap(),
            None
        );
        assert!(lock_owner(
            "1: FLOCK ADVISORY WRITE -1 00:2f:9007199254740993 0 EOF",
            &identity()
        )
        .is_err());
        assert!(lock_owner(
            "1: POSIX ADVISORY WRITE 123 00:2f:9007199254740993 0 EOF",
            &identity()
        )
        .is_err());
    }

    #[test]
    fn host_identity_rejects_symlinks_wrong_owners_and_lossy_inode_values() {
        let data = format!("{} 9007199254740993 81b6 0\n", identity().device);
        assert_eq!(parse_stat(&data).unwrap(), identity());
        assert!(parse_stat("47 123 a1ff 0").is_err());
        assert!(parse_stat("47 123 81b6 1000").is_err());
        assert_eq!(
            serde_json::to_value(identity()).unwrap()["inode"],
            "9007199254740993"
        );
    }

    #[test]
    fn replacement_changes_identity_and_symlinks_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        std::fs::write(&path, "stale PID text").unwrap();
        let held = File::open(&path).unwrap();
        let first = file_identity(&path, false).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "same PID text").unwrap();
        assert_ne!(file_identity(&path, false).unwrap(), first);
        drop(held);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        assert!(file_identity(&path, false).is_err());
    }
}
