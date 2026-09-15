use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MARKER: &str = ".catalog-removal.json";

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Removal {
    schema: u32,
    directory: String,
    root_device: u64,
    root_inode: u64,
    device: u64,
    inode: u64,
}

fn pinned(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn parent(config: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(config)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "Catalog recovery requires a private daemon-owned state directory"
    );
    Ok(file)
}

fn record(root: &File, selected: &File, name: &str) -> Result<Removal> {
    super::catalog::validate_relative_path(name)?;
    ensure!(
        !name.contains('/') && name.starts_with("catalog-"),
        "Invalid removal directory name"
    );
    let root = root.metadata()?;
    let selected = selected.metadata()?;
    Ok(Removal {
        schema: 1,
        directory: name.into(),
        root_device: root.dev(),
        root_inode: root.ino(),
        device: selected.dev(),
        inode: selected.ino(),
    })
}

fn read(parent: &File) -> Result<Option<Removal>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(pinned(parent).join(MARKER))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.len() <= 4096
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Catalog recovery marker is not a private regular file"
    );
    let mut bytes = Vec::new();
    File::open(pinned(&file))?
        .take(4097)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "Catalog recovery marker exceeds 4 KiB");
    let removal: Removal = serde_json::from_slice(&bytes)?;
    super::catalog::validate_relative_path(&removal.directory)?;
    ensure!(
        removal.schema == 1
            && !removal.directory.contains('/')
            && removal.directory.starts_with("catalog-"),
        "Invalid catalog recovery marker"
    );
    Ok(Some(removal))
}

pub(super) fn prepare(config: &Path, root: &File, selected: &File, name: &str) -> Result<()> {
    let parent = parent(config)?;
    let removal = record(root, selected, name)?;
    if let Some(previous) = read(&parent)? {
        if previous == removal {
            return parent
                .sync_all()
                .context("syncing existing catalog recovery marker");
        }
        ensure!(
            previous.root_device == removal.root_device
                && previous.root_inode == removal.root_inode,
            "Catalog recovery belongs to a different storage directory"
        );
        match fs::symlink_metadata(pinned(root).join(&previous.directory)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => anyhow::bail!(
                "Review and finish the previous removal of '{}' first",
                previous.directory
            ),
        }
        fs::remove_file(pinned(&parent).join(MARKER))?;
        parent.sync_all()?;
    }
    let mut staged = tempfile::NamedTempFile::new_in(pinned(&parent))?;
    staged.write_all(&serde_json::to_vec(&removal)?)?;
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(pinned(&parent).join(MARKER))
        .map_err(|error| error.error)
        .context("publishing catalog recovery marker")?;
    parent.sync_all().context("syncing catalog recovery marker")
}

pub(super) fn verify_empty(config: &Path, root: &File, selected: &File, name: &str) -> Result<()> {
    let parent = parent(config)?;
    ensure!(
        read(&parent)?.as_ref() == Some(&record(root, selected, name)?),
        "No matching interrupted-removal marker"
    );
    let metadata = selected.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "Interrupted removal directory has insecure ownership"
    );
    ensure!(
        fs::read_dir(pinned(selected))?.next().is_none(),
        "Interrupted removal directory is not empty. Preserve it for manual review"
    );
    Ok(())
}

pub(super) fn finish(config: &Path, root: &File, selected: &File, name: &str) -> Result<()> {
    let parent = parent(config)?;
    ensure!(
        read(&parent)?.as_ref() == Some(&record(root, selected, name)?),
        "Catalog recovery marker changed during removal"
    );
    fs::remove_file(pinned(&parent).join(MARKER))?;
    parent
        .sync_all()
        .context("syncing catalog recovery completion")
}
