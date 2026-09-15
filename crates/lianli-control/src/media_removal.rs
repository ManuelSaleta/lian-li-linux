use crate::media_ownership::{self, Receipt};
use crate::media_publication::{self, Directory};
use anyhow::{ensure, Context, Result};
use lianli_shared::template::catalog::CatalogDirectoryReview;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    sha256: String,
}

impl FileIdentity {
    fn matches(&self, metadata: &Metadata) -> bool {
        // Removing a hard-linked alias changes ctime and link count on surviving names.
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.ino() == self.inode
            && metadata.len() == self.bytes
            && (metadata.mtime(), metadata.mtime_nsec()) == self.modified
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    receipt: Receipt,
    files: BTreeMap<String, FileIdentity>,
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_name(name: &str) -> bool {
    let (hash, extension) = name
        .split_once('.')
        .map_or((name, None), |(a, b)| (a, Some(b)));
    valid_hash(hash)
        && extension.is_none_or(|value| {
            !value.is_empty()
                && value.len() <= 16
                && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn validate(journal: &Journal, id: &str) -> Result<()> {
    media_ownership::validate_id(id)?;
    ensure!(
        journal.version == 1
            && journal.receipt.version == 1
            && journal.receipt.id == id
            && valid_hash(&journal.receipt.fingerprint)
            && journal.files.len() <= 8192,
        "Invalid managed removal record"
    );
    for (name, file) in &journal.files {
        ensure!(
            valid_name(name) && valid_hash(&file.sha256) && file.bytes <= 2 * 1024 * 1024 * 1024,
            "Invalid managed removal file record"
        );
    }
    Ok(())
}

fn read(directory: &Directory, id: &str) -> Result<Option<Journal>> {
    media_ownership::validate_id(id)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(directory.path().join(format!("{id}.json")))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Opening managed removal record"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= MAX_JOURNAL_BYTES,
        "Managed removal record must be a bounded private regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_JOURNAL_BYTES,
        "Managed removal record is too large"
    );
    let journal: Journal = serde_json::from_slice(&bytes)?;
    validate(&journal, id)?;
    Ok(Some(journal))
}

pub struct PreparedRemoval {
    imports: Directory,
    directory: Option<Directory>,
    receipts: Directory,
    removals: Directory,
    journal: Journal,
}

impl PreparedRemoval {
    pub fn review(&self) -> Result<CatalogDirectoryReview> {
        self.verify_remaining()?;
        let mut review = if let Some(directory) = &self.directory {
            media_ownership::review_directory(directory, &self.journal.receipt.id)?
        } else {
            CatalogDirectoryReview {
                directory: self.journal.receipt.id.clone(),
                sha256: self.fingerprint()?,
                bytes: 0,
                files: Vec::new(),
                missing_files: Vec::new(),
            }
        };
        self.verify_review(&review)?;
        let present: std::collections::BTreeSet<_> =
            review.files.iter().map(|file| file.path.as_str()).collect();
        review.missing_files = self
            .journal
            .files
            .keys()
            .filter(|name| !present.contains(name.as_str()))
            .cloned()
            .collect();
        Ok(review)
    }

    fn fingerprint(&self) -> Result<String> {
        if let Some(directory) = &self.directory {
            return media_publication::fingerprint_directory(directory);
        }
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"lianli-removed-import-v1\0");
        digest.update(serde_json::to_vec(&self.journal)?);
        Ok(format!("{:x}", digest.finalize()))
    }

    pub fn verify_remaining(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let id = &self.journal.receipt.id;
        let current = optional_directory(&self.imports, id)?;
        let receipt = media_ownership::read(&self.receipts, id)?;
        ensure!(
            self.imports.0.metadata()?.ino() == self.journal.receipt.imports_inode
                && receipt
                    .as_ref()
                    .is_none_or(|receipt| receipt == &self.journal.receipt)
                && (self.directory.is_none() || receipt.is_some()),
            "Managed removal ownership changed. Preserve remaining storage"
        );
        match (&self.directory, current) {
            (Some(directory), Some(current)) => ensure!(
                directory.0.metadata()?.ino() == self.journal.receipt.directory_inode
                    && current.0.metadata()?.ino() == self.journal.receipt.directory_inode,
                "Managed directory was replaced"
            ),
            (None, None) => {}
            _ => anyhow::bail!("Managed directory changed during removal"),
        }
        if let Some(directory) = &self.directory {
            media_publication::fingerprint_directory(directory)?;
            for (index, entry) in fs::read_dir(directory.path())?.enumerate() {
                ensure!(
                    index < 8192 && Instant::now() < deadline,
                    "Managed removal inspection exceeds its entry or time limit"
                );
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("Invalid remaining filename"))?;
                let expected = self
                    .journal
                    .files
                    .get(&name)
                    .context("Unexpected file in managed removal directory")?;
                ensure!(
                    expected.matches(&fs::symlink_metadata(entry.path())?),
                    "Managed file changed after removal was prepared"
                );
            }
        }
        ensure!(
            read(&self.removals, id)?.as_ref() == Some(&self.journal),
            "Managed removal record changed"
        );
        ensure!(
            Instant::now() < deadline,
            "Managed removal inspection exceeded five seconds"
        );
        Ok(())
    }

    pub fn verify_review(&self, review: &CatalogDirectoryReview) -> Result<()> {
        self.verify_remaining()?;
        let mut remaining = std::collections::BTreeSet::new();
        let entries = self
            .directory
            .as_ref()
            .map(|directory| fs::read_dir(directory.path()))
            .transpose()?;
        for entry in entries.into_iter().flatten() {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Invalid remaining filename"))?;
            ensure!(remaining.len() < 8192, "Too many remaining managed files");
            remaining.insert(name);
        }
        ensure!(
            review.directory == self.journal.receipt.id && review.sha256 == self.fingerprint()?,
            "Managed contents changed since review"
        );
        for file in &review.files {
            ensure!(
                remaining.remove(&file.path),
                "Review contains a missing or duplicated managed file"
            );
            let expected = self
                .journal
                .files
                .get(&file.path)
                .context("Unrecorded managed file")?;
            ensure!(
                file.sha256 == expected.sha256 && file.bytes == expected.bytes,
                "Managed content changed after removal was prepared"
            );
        }
        ensure!(
            remaining.is_empty(),
            "Review omitted remaining managed files"
        );
        Ok(())
    }
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

/// The caller must exclude configuration writes, publication and new media preparation until return.
pub fn remove(
    config_dir: &Path,
    id: &str,
    fingerprint: &str,
    authorize: impl FnOnce() -> Result<()>,
) -> Result<()> {
    remove_with(config_dir, id, fingerprint, authorize, || Ok(()))
}

pub(crate) fn remove_with(
    config_dir: &Path,
    id: &str,
    fingerprint: &str,
    authorize: impl FnOnce() -> Result<()>,
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<()> {
    let review = media_ownership::review(config_dir, id)?;
    ensure!(
        review.sha256 == fingerprint,
        "Managed contents changed. Review again"
    );
    authorize()?;
    if resume(config_dir, id)?.is_none() {
        let media = Directory::open(config_dir)?.child("media", false)?;
        if let Some(orphan) = orphan_review_media(&media, id)? {
            ensure!(
                orphan.sha256 == fingerprint,
                "Managed orphan receipt changed. Review again"
            );
            let receipts = media.child("receipts", false)?;
            fs::remove_file(receipts.path().join(format!("{id}.json")))?;
            receipts.0.sync_all()?;
            return Ok(());
        }
    }
    let mut prepared = prepare(config_dir, &review)?;
    prepared.verify_review(&review)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    if let Some(directory) = &prepared.directory {
        let mut files: Vec<_> = review.files.iter().collect();
        // Keep each extensionless object until its hard-linked aliases have been removed.
        files.sort_by_key(|file| !file.path.contains('.'));
        for file in files {
            ensure!(
                Instant::now() < deadline,
                "Managed removal timed out. Review remaining files again"
            );
            let target = directory.path().join(&file.path);
            let expected = prepared
                .journal
                .files
                .get(&file.path)
                .context("Missing reviewed file record")?;
            ensure!(
                expected.matches(&fs::symlink_metadata(&target)?),
                "Managed file changed before removal"
            );
            fs::remove_file(target)?;
            checkpoint()?;
        }
        directory.0.sync_all()?;
        prepared.verify_remaining()?;
        fs::remove_dir(prepared.imports.path().join(id))?;
        prepared.imports.0.sync_all()?;
        prepared.directory = None;
        checkpoint()?;
    }
    prepared.verify_remaining()?;
    let receipt_path = prepared.receipts.path().join(format!("{id}.json"));
    match fs::remove_file(receipt_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Retiring managed ownership receipt"),
    }
    prepared.receipts.0.sync_all()?;
    checkpoint()?;
    prepared.verify_remaining()?;
    fs::remove_file(prepared.removals.path().join(format!("{id}.json")))?;
    prepared.removals.0.sync_all()?;
    Ok(())
}

pub fn resume(config_dir: &Path, id: &str) -> Result<Option<PreparedRemoval>> {
    media_ownership::validate_id(id)?;
    let media = Directory::open(config_dir)?.child("media", false)?;
    let removals = match media.child("removals", false) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let Some(journal) = read(&removals, id)? else {
        return Ok(None);
    };
    let imports = media.child("imports", false)?;
    let prepared = PreparedRemoval {
        directory: optional_directory(&imports, id)?,
        receipts: media.child("receipts", false)?,
        imports,
        removals,
        journal,
    };
    prepared.verify_remaining()?;
    Ok(Some(prepared))
}

pub(crate) fn pending_ids(media: &Directory) -> Result<Vec<String>> {
    let Some(removals) = optional_directory(media, "removals")? else {
        return Ok(Vec::new());
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut ids = Vec::new();
    for entry in fs::read_dir(removals.path())? {
        ensure!(
            ids.len() < 1024 && Instant::now() < deadline,
            "Managed removal inventory exceeded its limits"
        );
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid managed removal record name"))?;
        let id = name
            .strip_suffix(".json")
            .context("Unexpected managed removal metadata")?;
        media_ownership::validate_id(id)?;
        ids.push(id.into());
    }
    Ok(ids)
}

pub(crate) fn orphan_review(config_dir: &Path, id: &str) -> Result<Option<CatalogDirectoryReview>> {
    media_ownership::validate_id(id)?;
    let media = Directory::open(config_dir)?.child("media", false)?;
    orphan_review_media(&media, id)
}

fn orphan_review_media(media: &Directory, id: &str) -> Result<Option<CatalogDirectoryReview>> {
    if let Some(imports) = optional_directory(media, "imports")? {
        if optional_directory(&imports, id)?.is_some() {
            return Ok(None);
        }
    }
    let Some(receipts) = optional_directory(media, "receipts")? else {
        return Ok(None);
    };
    let Some(receipt) = media_ownership::read(&receipts, id)? else {
        return Ok(None);
    };
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"lianli-orphan-receipt-v1\0");
    digest.update(serde_json::to_vec(&receipt)?);
    Ok(Some(CatalogDirectoryReview {
        directory: id.into(),
        sha256: format!("{:x}", digest.finalize()),
        bytes: 0,
        files: Vec::new(),
        missing_files: Vec::new(),
    }))
}

pub fn prepare(config_dir: &Path, review: &CatalogDirectoryReview) -> Result<PreparedRemoval> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let id = &review.directory;
    if let Some(prepared) = resume(config_dir, id)? {
        prepared.verify_review(review)?;
        return Ok(prepared);
    }
    ensure!(
        media_ownership::verify(config_dir, id)?,
        "Managed ownership is unverified"
    );
    let media = Directory::open(config_dir)?.child("media", false)?;
    let imports = media.child("imports", false)?;
    let directory = imports.child(id, false)?;
    let receipts = media.child("receipts", false)?;
    let receipt =
        media_ownership::read(&receipts, id)?.context("Managed ownership receipt disappeared")?;
    ensure!(
        review.sha256 == media_publication::fingerprint_directory(&directory)?,
        "Managed import changed since review"
    );
    let mut files = BTreeMap::new();
    for file in &review.files {
        ensure!(
            files.len() < 8192 && Instant::now() < deadline,
            "Managed removal preparation exceeds its entry or time limit"
        );
        ensure!(valid_name(&file.path), "Invalid reviewed managed filename");
        let metadata = fs::symlink_metadata(directory.path().join(&file.path))?;
        let identity = FileIdentity {
            inode: metadata.ino(),
            bytes: file.bytes,
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            sha256: file.sha256.clone(),
        };
        ensure!(
            identity.matches(&metadata) && files.insert(file.path.clone(), identity).is_none(),
            "Invalid or duplicated reviewed managed file"
        );
    }
    let journal = Journal {
        version: 1,
        receipt,
        files,
    };
    validate(&journal, id)?;
    for (index, entry) in fs::read_dir(directory.path())?.enumerate() {
        ensure!(
            index < 8192 && Instant::now() < deadline,
            "Managed removal preparation exceeded its limits"
        );
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid managed filename"))?;
        ensure!(
            journal.files.contains_key(&name),
            "Review omitted a managed file"
        );
    }
    ensure!(
        media_publication::fingerprint_directory(&directory)? == review.sha256,
        "Managed import changed while preparing removal"
    );
    let removals = media.child("removals", true)?;
    for (index, entry) in fs::read_dir(removals.path())?.enumerate() {
        entry?;
        ensure!(
            index < 1023 && Instant::now() < deadline,
            "Managed removal records exceed the entry or time limit"
        );
    }
    let bytes = serde_json::to_vec(&journal)?;
    ensure!(
        bytes.len() as u64 <= MAX_JOURNAL_BYTES,
        "Managed removal record is too large"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(removals.path())?;
    temporary.write_all(&bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(removals.path().join(format!("{id}.json")))?;
    removals.0.sync_all()?;
    let prepared = PreparedRemoval {
        imports,
        directory: Some(directory),
        receipts,
        removals,
        journal,
    };
    prepared.verify_remaining()?;
    Ok(prepared)
}
