use crate::media_publication::{self, Directory};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub version: u32,
    pub id: String,
    pub imports_inode: u64,
    pub directory_inode: u64,
    pub fingerprint: String,
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid managed import ID"
    );
    Ok(())
}

pub(crate) fn read(directory: &Directory, id: &str) -> Result<Option<Receipt>> {
    validate_id(id)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(directory.path().join(format!("{id}.json")))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Opening managed import ownership receipt"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= 4096,
        "Managed import receipt must be a private regular file of at most 4 KiB"
    );
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "Managed import receipt exceeds 4 KiB");
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.version == 1
            && receipt.id == id
            && receipt.fingerprint.len() == 64
            && receipt
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Invalid managed import ownership receipt"
    );
    Ok(Some(receipt))
}

pub(crate) fn record(
    media: &Directory,
    imports: &Directory,
    staged: &Directory,
    id: &str,
    fingerprint: &str,
) -> Result<()> {
    validate_id(id)?;
    ensure!(
        fingerprint.len() == 64 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid managed media fingerprint"
    );
    let receipts = media.child("receipts", true)?;
    let receipt = Receipt {
        version: 1,
        id: id.into(),
        imports_inode: imports.0.metadata()?.ino(),
        directory_inode: staged.0.metadata()?.ino(),
        fingerprint: fingerprint.into(),
    };
    if let Some(previous) = read(&receipts, id)? {
        ensure!(
            previous == receipt,
            "Managed import ID already belongs to another publication"
        );
        receipts.0.sync_all()?;
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    for (index, entry) in fs::read_dir(receipts.path())?.enumerate() {
        entry?;
        ensure!(index < 2047 && Instant::now() < deadline, "Managed import receipts exceed the entry or time limit. Review unused imports before copying more");
    }
    let mut temporary = tempfile::NamedTempFile::new_in(receipts.path())?;
    serde_json::to_writer(&mut temporary, &receipt)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(receipts.path().join(format!("{id}.json")))?;
    receipts.0.sync_all()?;
    Ok(())
}

/// Missing receipts return false; changed or invalid imports return an error.
pub fn verify(config_dir: &Path, id: &str) -> Result<bool> {
    validate_id(id)?;
    let root = Directory::open(config_dir)?;
    let media = root.child("media", false)?;
    let receipts = match media.child("receipts", false) {
        Ok(receipts) => receipts,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let Some(receipt) = read(&receipts, id)? else {
        return Ok(false);
    };
    let imports = media.child("imports", false)?;
    let directory = imports.child(id, false)?;
    ensure!(
        imports.0.metadata()?.ino() == receipt.imports_inode
            && directory.0.metadata()?.ino() == receipt.directory_inode
            && media_publication::fingerprint_directory(&directory)? == receipt.fingerprint,
        "Managed import changed since publication. Automatic cleanup is not authorized"
    );
    Ok(true)
}

pub fn inspect(
    config_dir: &Path,
) -> Result<Vec<lianli_shared::template::catalog::CatalogStorageEntry>> {
    let root = Directory::open(config_dir)?;
    let media = match root.child("media", false) {
        Ok(media) => media,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(Vec::new())
        }
        Err(error) => return Err(error),
    };
    let imports = optional_directory(&media, "imports")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut entries = Vec::new();
    let mut files = 0usize;
    let directories = imports
        .as_ref()
        .map(|imports| fs::read_dir(imports.path()))
        .transpose()?;
    for entry in directories.into_iter().flatten() {
        ensure!(
            entries.len() < 1024 && Instant::now() < deadline,
            "Managed import inventory exceeds its directory or time limit"
        );
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Managed import name is not UTF-8"))?;
        let directory = Directory::open(&entry.path())?;
        let mut objects = std::collections::HashSet::new();
        let mut bytes = 0u64;
        for file in fs::read_dir(directory.path())? {
            files += 1;
            ensure!(
                files <= 65536 && Instant::now() < deadline,
                "Managed import inventory exceeds its file or time limit"
            );
            let metadata = fs::symlink_metadata(file?.path())?;
            ensure!(
                metadata.is_file(),
                "Managed import inventory refuses symlinks, nested directories and special files"
            );
            if objects.insert((metadata.dev(), metadata.ino())) {
                bytes = bytes
                    .checked_add(metadata.len())
                    .context("Managed import size overflow")?;
            }
        }
        let issue = match verify(config_dir, &name) {
            Ok(true) => None,
            Ok(false) => Some("No ownership receipt. Retained as unverified storage".into()),
            Err(error) => Some(format!("{error:#}").chars().take(2048).collect()),
        };
        entries.push(lianli_shared::template::catalog::CatalogStorageEntry {
            directory: name,
            bytes,
            ownership_verified: issue.is_none(),
            issue,
            ..Default::default()
        });
    }
    if let Some(receipts) = optional_directory(&media, "receipts")? {
        let published: std::collections::HashSet<_> = entries
            .iter()
            .map(|entry| entry.directory.clone())
            .collect();
        for (index, entry) in fs::read_dir(receipts.path())?.enumerate() {
            ensure!(
                index < 2048 && Instant::now() < deadline,
                "Managed receipt inventory exceeds its entry or time limit"
            );
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Managed receipt name is not UTF-8"))?;
            let id = name
                .strip_suffix(".json")
                .context("Unexpected file in managed receipt storage")?;
            validate_id(id)?;
            if published.contains(id) {
                continue;
            }
            ensure!(
                entries.len() < 1024,
                "Managed inventory exceeds 1024 entries"
            );
            let ownership_verified = crate::media_removal::orphan_review(config_dir, id)
                .is_ok_and(|review| review.is_some());
            let issue = match read(&receipts, id) {
                Ok(Some(_)) => "Ownership receipt has no published directory. An import may be pending or interrupted. Review metadata and references before removal.".into(),
                Ok(None) => anyhow::bail!("Managed receipt disappeared during inventory"),
                Err(error) => format!("Invalid retained ownership receipt: {error:#}")
                    .chars()
                    .take(2048)
                    .collect(),
            };
            entries.push(lianli_shared::template::catalog::CatalogStorageEntry {
                directory: id.into(),
                ownership_verified,
                issue: Some(issue),
                ..Default::default()
            });
        }
    }
    for id in crate::media_removal::pending_ids(&media)? {
        ensure!(
            Instant::now() < deadline,
            "Managed import inventory exceeded five seconds"
        );
        let position =
            if let Some(position) = entries.iter().position(|entry| entry.directory == id) {
                position
            } else {
                ensure!(
                    entries.len() < 1024,
                    "Managed inventory exceeds 1024 entries"
                );
                entries.push(lianli_shared::template::catalog::CatalogStorageEntry {
                    directory: id.clone(),
                    ..Default::default()
                });
                entries.len() - 1
            };
        let entry = &mut entries[position];
        match crate::media_removal::resume(config_dir, &id) {
            Ok(Some(_)) => {
                entry.ownership_verified = true;
                entry.issue = Some(
                    "Interrupted removal. Review remaining files and references before resuming"
                        .into(),
                );
            }
            Ok(None) => anyhow::bail!("Managed removal record disappeared during inventory"),
            Err(error) => {
                entry.ownership_verified = false;
                entry.issue = Some(format!("{error:#}").chars().take(2048).collect());
            }
        }
    }
    ensure!(
        Instant::now() < deadline,
        "Managed import inventory exceeded five seconds"
    );
    entries.sort_by(|left, right| left.directory.cmp(&right.directory));
    Ok(entries)
}

fn optional_directory(parent: &Directory, name: &str) -> Result<Option<Directory>> {
    match parent.child(name, false) {
        Ok(directory) => Ok(Some(directory)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn review(
    config_dir: &Path,
    id: &str,
) -> Result<lianli_shared::template::catalog::CatalogDirectoryReview> {
    if let Some(removal) = crate::media_removal::resume(config_dir, id)? {
        return removal.review();
    }
    if let Some(orphan) = crate::media_removal::orphan_review(config_dir, id)? {
        return Ok(orphan);
    }
    ensure!(
        verify(config_dir, id)?,
        "Managed import ownership is unverified"
    );
    let directory = Directory::open(config_dir)?
        .child("media", false)?
        .child("imports", false)?
        .child(id, false)?;
    let reviewed = review_directory(&directory, id)?;
    ensure!(
        verify(config_dir, id)?,
        "Managed import changed during content review"
    );
    Ok(reviewed)
}

pub(crate) fn review_directory(
    directory: &Directory,
    id: &str,
) -> Result<lianli_shared::template::catalog::CatalogDirectoryReview> {
    use sha2::{Digest, Sha256};
    let fingerprint = media_publication::fingerprint_directory(directory)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut hashes = std::collections::HashMap::new();
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; 128 * 1024];
    for entry in fs::read_dir(directory.path())? {
        ensure!(
            files.len() < 8192 && Instant::now() < deadline,
            "Managed import review exceeded its entry or time limit"
        );
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid managed filename"))?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(entry.path())?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 2 * 1024 * 1024 * 1024,
            "Managed review requires bounded regular files"
        );
        let key = (metadata.dev(), metadata.ino());
        let digest = if let Some(digest) = hashes.get(&key) {
            String::clone(digest)
        } else {
            ensure!(
                hashes.len() < 4096,
                "Managed review exceeds 4096 media objects"
            );
            let mut digest = Sha256::new();
            let mut size = 0u64;
            loop {
                ensure!(
                    Instant::now() < deadline,
                    "Managed review exceeded ten minutes"
                );
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                size += count as u64;
                ensure!(size <= metadata.len(), "Managed media grew during review");
                digest.update(&buffer[..count]);
            }
            ensure!(size == metadata.len(), "Managed media shrank during review");
            bytes += size;
            ensure!(
                bytes <= 8 * 1024 * 1024 * 1024,
                "Managed review exceeds 8 GiB"
            );
            let digest = format!("{:x}", digest.finalize());
            hashes.insert(key, digest.clone());
            digest
        };
        files.push(lianli_shared::template::catalog::CatalogReviewedFile {
            matches_catalog: Some(name.split('.').next() == Some(digest.as_str())),
            path: name,
            bytes: metadata.len(),
            sha256: digest,
        });
    }
    ensure!(
        media_publication::fingerprint_directory(directory)? == fingerprint,
        "Managed import changed during content review"
    );
    ensure!(
        Instant::now() < deadline,
        "Managed review exceeded ten minutes"
    );
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(lianli_shared::template::catalog::CatalogDirectoryReview {
        directory: id.into(),
        sha256: fingerprint,
        bytes,
        files,
        missing_files: Vec::new(),
    })
}
