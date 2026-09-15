use crate::account::Account;
use crate::destination::Destination;
use crate::media_staging::{CopyControl, MediaStaging, SourceIdentity};
use crate::state::{StateSnapshot, StateSummary};
use crate::transfer_channel::Channel;
use anyhow::{ensure, Context, Result};
use lianli_shared::services::ServiceScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

const MAX_FILE: usize = 16 * 1024 * 1024;
const MAX_STATE: usize = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(600);
const PREFIX: &str = ".lianli-migration-";

#[derive(Serialize, Deserialize)]
enum Message {
    Begin {
        generation: String,
        config_name: String,
        assets: usize,
        files: usize,
    },
    Asset {
        path: PathBuf,
    },
    State {
        path: PathBuf,
        bytes: usize,
        digest: String,
    },
    Finish,
    Accepted,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Ready,
    Copied(PathBuf),
    StateAccepted,
    Prepared,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedTransfer {
    version: u32,
    pub id: String,
    pub destination_uid: u32,
    pub config_path: PathBuf,
    pub working_directory: PathBuf,
    pub source_generation: String,
    pub destination_generation: Option<String>,
    #[serde(default)]
    pub state_generation: Option<String>,
    #[serde(default)]
    pub media_generation: Option<String>,
    #[serde(default)]
    pub decode_validated: bool,
    pub directory: String,
    pub media_directory: String,
    pub final_media_path: PathBuf,
    pub state_files: usize,
    pub media_files: usize,
    pub media_bytes: u64,
}

pub fn prepare_native(scope: ServiceScope) -> Result<PreparedTransfer> {
    use lianli_shared::installation::InstallationContext;
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Automatic transfer requires the native host application"
    );
    let caller = Account::authorized_caller()?;
    let system = Account::system()?;
    let (source, source_scope, destination) = match scope {
        ServiceScope::User => (system, ServiceScope::System, caller),
        ServiceScope::System => (caller, ServiceScope::User, system),
    };
    let operation =
        crate::reservation::ServiceOperationLock::acquire(&InstallationContext::Native)?;
    let source_state = crate::destination::preflight(&source, source_scope)?;
    let destination_state = crate::destination::preflight(&destination, scope)?;
    let mut random = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    prepare(
        &source,
        &source_state,
        &destination,
        &destination_state,
        &id,
        &operation,
    )
}

pub fn prepare(
    source: &Account,
    source_state: &Destination,
    destination: &Account,
    destination_state: &Destination,
    id: &str,
    operation: &crate::reservation::ServiceOperationLock,
) -> Result<PreparedTransfer> {
    operation.verify()?;
    let result = prepare_inner(source, source_state, destination, destination_state, id).and_then(
        |prepared| {
            operation.verify()?;
            Ok(prepared)
        },
    );
    match result {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            if let Err(cleanup) = discard_as(destination, &destination_state.config_path, id) {
                return Err(error).context(format!("Preparation cleanup also failed: {cleanup:#}. Inspect pending destination state before retrying"));
            }
            Err(error)
        }
    }
}

fn prepare_inner(
    source: &Account,
    source_state: &Destination,
    destination: &Account,
    destination_state: &Destination,
    id: &str,
) -> Result<PreparedTransfer> {
    ensure!(
        std::env::current_exe()?.file_name() == Some(OsStr::new("lianli-control")),
        "Run state transfer through the standalone control helper"
    );
    validate_id(id)?;
    ensure!(
        source.uid == source_state.uid
            && destination.uid == destination_state.uid
            && source.group_fingerprint() == source_state.groups_fingerprint
            && destination.group_fingerprint() == destination_state.groups_fingerprint
            && source_state.mount_namespace == destination_state.mount_namespace,
        "State transfer accounts or namespaces differ from preflight"
    );
    let generation = &source_state
        .state
        .as_ref()
        .context("Save source configuration before carrying settings")?
        .generation;
    let destination_generation = destination_state
        .state
        .as_ref()
        .map(|state| state.generation.as_str())
        .unwrap_or("fresh");
    let source_command = source.control_command(&[
        OsStr::new("send-state"),
        OsStr::new("--config"),
        source_state.config_path.as_os_str(),
        OsStr::new("--working-directory"),
        source_state.working_directory.as_os_str(),
        OsStr::new("--generation"),
        OsStr::new(generation),
    ])?;
    let destination_command = destination.control_command(&[
        OsStr::new("receive-state"),
        OsStr::new("--scope"),
        OsStr::new(match destination_state.scope {
            ServiceScope::User => "user",
            ServiceScope::System => "system",
        }),
        OsStr::new("--operation-id"),
        OsStr::new(id),
        OsStr::new("--generation"),
        OsStr::new(destination_generation),
    ])?;
    let (send_fd, receive_fd) = Channel::pair()?;
    let (sent, received) = std::thread::scope(|scope| {
        let sender = scope.spawn(move || {
            crate::command::run_with_stdin(source_command, Stdio::from(send_fd), TIMEOUT)
        });
        let received =
            crate::command::run_with_stdin(destination_command, Stdio::from(receive_fd), TIMEOUT);
        let sent = sender
            .join()
            .map_err(|_| anyhow::anyhow!("Source transfer supervisor panicked"));
        (sent, received)
    });
    let (sent, received) = successful_outputs(sent.and_then(|result| result), received)?;
    let prepared: PreparedTransfer = serde_json::from_str(&received.stdout)?;
    validate_preparation(&prepared, &destination_state.config_path, id)?;
    let source_summary: StateSummary = serde_json::from_str(&sent.stdout)?;
    ensure!(
        source_summary.generation == *generation
            && source_summary.files == prepared.state_files
            && prepared.source_generation == *generation
            && prepared.destination_generation.as_deref()
                == destination_state
                    .state
                    .as_ref()
                    .map(|state| state.generation.as_str())
            && prepared.destination_uid == destination.uid
            && prepared.config_path == destination_state.config_path
            && prepared.working_directory == destination_state.working_directory
            && prepared.id == id,
        "Prepared state does not match the authorized account and generation"
    );
    Ok(prepared)
}

fn successful_outputs(
    source: Result<crate::command::Output>,
    destination: Result<crate::command::Output>,
) -> Result<(crate::command::Output, crate::command::Output)> {
    let checked =
        |result: Result<crate::command::Output>, side| -> Result<crate::command::Output> {
            let output = result.with_context(|| format!("{side} transfer helper failed"))?;
            ensure!(
                output.status.success(),
                "{side} transfer failed: {}",
                output.stderr.trim().chars().take(2048).collect::<String>()
            );
            Ok(output)
        };
    match (
        checked(source, "Source"),
        checked(destination, "Destination"),
    ) {
        (Ok(source), Ok(destination)) => Ok((source, destination)),
        (Err(source), Err(destination)) => anyhow::bail!("{source:#}. {destination:#}"),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

pub(crate) fn validate_preparation(
    prepared: &PreparedTransfer,
    config: &Path,
    id: &str,
) -> Result<()> {
    validate_id(id)?;
    let generated = |name: &str, prefix: &str| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            (6..=32).contains(&suffix.len())
                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    };
    ensure!(
        prepared.version == 1
            && prepared.id == id
            && prepared.config_path == config
            && generated(&prepared.directory, &format!("{PREFIX}{id}-"))
            && generated(&prepared.media_directory, ".lianli-media-")
            && prepared.final_media_path
                == config
                    .parent()
                    .context("Invalid destination config path")?
                    .join("media/imports")
                    .join(id)
            && (1..=259).contains(&prepared.state_files)
            && prepared.media_files <= 4096
            && prepared.media_bytes <= 8 * 1024 * 1024 * 1024
            && [&prepared.state_generation, &prepared.media_generation]
                .into_iter()
                .all(|value| value
                    .as_ref()
                    .is_none_or(|value| value.len() == 64
                        && value.bytes().all(|byte| byte.is_ascii_hexdigit()))),
        "Invalid or unsupported prepared transfer paths and limits"
    );
    Ok(())
}

pub fn send(config: &Path, working: &Path, generation: &str) -> Result<StateSummary> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Source files must be opened under an unprivileged account"
    );
    let channel = inherited_channel()?;
    let snapshot = StateSnapshot::read(config, working)?;
    ensure!(
        snapshot.summary.generation == generation,
        "Source state changed after preflight"
    );
    send_snapshot(&channel, snapshot, config, working)
}

fn send_snapshot(
    channel: &Channel,
    snapshot: StateSnapshot,
    config: &Path,
    working: &Path,
) -> Result<StateSummary> {
    ensure!(
        snapshot.summary.issue_count == 0,
        "Source settings contain invalid saved state"
    );
    let control = CopyControl::new(TIMEOUT);
    let access = snapshot.check_assets(&control)?;
    ensure!(
        access.failed == 0,
        "Source assets became inaccessible after preflight"
    );
    let mut paths: Vec<_> = snapshot
        .dependencies
        .iter()
        .map(|dependency| dependency.path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    let config_name = config
        .file_name()
        .and_then(OsStr::to_str)
        .context("Invalid source config filename")?;
    channel.send(
        &Message::Begin {
            generation: snapshot.summary.generation.clone(),
            config_name: config_name.into(),
            assets: paths.len(),
            files: snapshot.files.len(),
        },
        None,
    )?;
    ensure!(
        matches!(reply(channel)?, Reply::Ready),
        "Destination did not accept state transfer"
    );
    let mut mapped = HashMap::new();
    let mut identities = Vec::new();
    for path in paths {
        control.check()?;
        let file = crate::state::open_media_source(&path)?;
        let identity = SourceIdentity::from(&file.metadata()?);
        channel.send(&Message::Asset { path: path.clone() }, Some(file.as_fd()))?;
        let Reply::Copied(destination) = reply(channel)? else {
            anyhow::bail!("Destination did not acknowledge copied media")
        };
        ensure!(
            SourceIdentity::from(&file.metadata()?) == identity,
            "Source media changed during transfer"
        );
        mapped.insert(path.clone(), destination);
        identities.push((path, identity));
    }
    let files = snapshot.rewrite_paths(&mapped)?;
    for file in files {
        control.check()?;
        let descriptor = sealed_state(&file.bytes)?;
        channel.send(
            &Message::State {
                path: file.relative_path,
                bytes: file.bytes.len(),
                digest: digest(&file.bytes),
            },
            Some(descriptor.as_fd()),
        )?;
        ensure!(
            matches!(reply(channel)?, Reply::StateAccepted),
            "Destination did not acknowledge saved state"
        );
    }
    verify_sources(&identities, &control)?;
    snapshot.verify_unchanged(config, working)?;
    channel.send(&Message::Finish, None)?;
    ensure!(
        matches!(reply(channel)?, Reply::Prepared),
        "Destination did not finish preparing state"
    );
    verify_sources(&identities, &control)?;
    snapshot.verify_unchanged(config, working)?;
    channel.send(&Message::Accepted, None)?;
    Ok(snapshot.summary)
}

pub fn receive(
    scope: ServiceScope,
    id: &str,
    expected_generation: &str,
) -> Result<PreparedTransfer> {
    let channel = inherited_channel()?;
    let destination = crate::destination::inspect(scope, false)?;
    let generation = destination
        .state
        .as_ref()
        .map(|state| state.generation.as_str())
        .unwrap_or("fresh");
    ensure!(
        generation == expected_generation,
        "Destination changed after preflight"
    );
    receive_into(
        &channel,
        &destination,
        id,
        crate::media_validation::ensure_decodable,
    )
}

fn receive_into(
    channel: &Channel,
    destination: &Destination,
    id: &str,
    validate: impl FnOnce(&[lianli_shared::media_dependencies::AssetDependency]) -> Result<()>,
) -> Result<PreparedTransfer> {
    validate_id(id)?;
    let (message, descriptor): (Message, _) = channel.receive()?;
    ensure!(
        descriptor.is_none(),
        "Unexpected descriptor in state transfer introduction"
    );
    let Message::Begin {
        generation,
        config_name,
        assets,
        files,
    } = message
    else {
        anyhow::bail!("Missing state transfer introduction")
    };
    ensure!(
        generation.len() == 64
            && generation.bytes().all(|byte| byte.is_ascii_hexdigit())
            && assets <= 4096
            && (1..=259).contains(&files),
        "Invalid state transfer limits or generation"
    );
    validate_config_name(&config_name)?;
    let parent_path = destination
        .config_path
        .parent()
        .context("Destination configuration has no directory")?;
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent_path)?;
    let metadata = parent.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.uid() == destination.uid
            && metadata.mode() & 0o022 == 0,
        "Destination state directory ownership changed"
    );
    let parent_fd = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    for (index, entry) in fs::read_dir(&parent_fd)?.enumerate() {
        ensure!(index < 4096, "Destination has too many state entries");
        ensure!(!entry?.file_name().to_string_lossy().starts_with(PREFIX),
            "A pending migration preparation already exists. Inspect or discard it before copying again");
    }
    let stage = tempfile::Builder::new()
        .prefix(&format!("{PREFIX}{id}-"))
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(&parent_fd)?;
    let final_media_path = parent_path.join("media/imports").join(id);
    let mut media = MediaStaging::new(stage.path(), &final_media_path)?;
    let control = CopyControl::new(TIMEOUT);
    channel.send(&Reply::Ready, None)?;
    for _ in 0..assets {
        control.check()?;
        let (message, descriptor): (Message, _) = channel.receive()?;
        let Message::Asset { path } = message else {
            anyhow::bail!("Expected the next media descriptor")
        };
        let descriptor = descriptor.context("Missing media descriptor")?;
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY && flags & libc::O_PATH == 0,
            "Media transfer requires a readable, read-only descriptor"
        );
        let copied = media.add(&path, &File::from(descriptor), &control)?;
        channel.send(&Reply::Copied(copied), None)?;
    }
    ensure!(
        media.paths().len() == assets,
        "Duplicate media descriptors in state transfer"
    );
    let mut saved = BTreeMap::new();
    let mut total = 0;
    for _ in 0..files {
        control.check()?;
        let (message, descriptor): (Message, _) = channel.receive()?;
        let Message::State {
            path,
            bytes,
            digest: expected,
        } = message
        else {
            anyhow::bail!("Expected the next saved state descriptor")
        };
        crate::state_transaction::validate_path(&path, &config_name)?;
        ensure!(
            bytes <= MAX_FILE && bytes <= MAX_STATE - total,
            "Transferred state exceeds its size budget"
        );
        let data = read_sealed_state(
            descriptor.context("Missing saved state descriptor")?,
            bytes,
            &expected,
        )?;
        let path = if path == Path::new(&config_name) {
            PathBuf::from(
                destination
                    .config_path
                    .file_name()
                    .context("Invalid destination config filename")?,
            )
        } else {
            path
        };
        ensure!(
            saved.insert(path, data).is_none(),
            "Duplicate saved state entry"
        );
        total += bytes;
        channel.send(&Reply::StateAccepted, None)?;
    }
    let (message, descriptor): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::Finish) && descriptor.is_none(),
        "Source did not finish generation checks"
    );
    let state_path = stage.path().join("state");
    fs::DirBuilder::new().mode(0o700).create(&state_path)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(state_path.join("profiles"))?;
    for (path, bytes) in &saved {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(state_path.join(path))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    let staged_config = state_path.join(destination.config_path.file_name().unwrap());
    let state = StateSnapshot::read(&staged_config, &destination.working_directory)?;
    ensure!(
        state.summary.issue_count == 0,
        "Transferred configuration is invalid: {}",
        state.summary.issues.join("\n")
    );
    let prepared_paths: std::collections::HashSet<_> = media.paths().values().collect();
    ensure!(
        state
            .dependencies
            .iter()
            .all(|dependency| prepared_paths.contains(&dependency.path)),
        "Transferred configuration references media outside this preparation"
    );
    let staged_dependencies = state
        .dependencies
        .iter()
        .map(|dependency| {
            let mut dependency = dependency.clone();
            dependency.path = media.directory().join(dependency.path.file_name().unwrap());
            dependency
        })
        .collect::<Vec<_>>();
    validate(&staged_dependencies)?;
    verify_destination_unchanged(destination)?;
    let mut prepared = PreparedTransfer {
        version: 1,
        id: id.into(),
        destination_uid: destination.uid,
        config_path: destination.config_path.clone(),
        working_directory: destination.working_directory.clone(),
        source_generation: generation,
        destination_generation: destination
            .state
            .as_ref()
            .map(|state| state.generation.clone()),
        state_generation: Some(state_fingerprint(&state.files)),
        media_generation: Some(crate::media_publication::fingerprint(media.directory())?),
        decode_validated: true,
        directory: stage
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        media_directory: String::new(),
        final_media_path,
        state_files: saved.len(),
        media_files: media.unique_files(),
        media_bytes: media.stored_bytes(),
    };
    prepared.media_directory = media.retain()?;
    let manifest_bytes = serde_json::to_vec(&prepared)?;
    ensure!(
        manifest_bytes.len() <= 60 * 1024,
        "Prepared transfer metadata exceeds its output limit"
    );
    let mut manifest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.path().join("manifest.json"))?;
    manifest.write_all(&manifest_bytes)?;
    manifest.sync_all()?;
    File::open(state_path.join("profiles"))?.sync_all()?;
    File::open(&state_path)?.sync_all()?;
    File::open(stage.path())?.sync_all()?;
    channel.send(&Reply::Prepared, None)?;
    let (message, descriptor): (Message, _) = channel.receive()?;
    ensure!(
        matches!(message, Message::Accepted) && descriptor.is_none(),
        "Source did not accept prepared state"
    );
    let current = fs::symlink_metadata(parent_path)?;
    ensure!(
        current.is_dir() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "Destination directory changed during transfer"
    );
    verify_destination_unchanged(destination)?;
    let _ = stage.keep();
    parent.sync_all()?;
    Ok(prepared)
}

fn verify_sources(identities: &[(PathBuf, SourceIdentity)], control: &CopyControl) -> Result<()> {
    for (path, identity) in identities {
        control.check()?;
        ensure!(
            SourceIdentity::from(&crate::state::open_media_source(path)?.metadata()?) == *identity,
            "Source media changed after it was copied. Prepare the migration again"
        );
    }
    Ok(())
}

fn verify_destination_unchanged(destination: &Destination) -> Result<()> {
    verify_destination_generation(
        &destination.config_path,
        &destination.working_directory,
        destination
            .state
            .as_ref()
            .map(|state| state.generation.as_str()),
    )
}

fn verify_destination_generation(
    config: &Path,
    working: &Path,
    expected: Option<&str>,
) -> Result<()> {
    if let Some(expected) = expected {
        let current = StateSnapshot::read(config, working)?;
        ensure!(
            current.summary.generation == expected,
            "Destination state changed while preparing migration"
        );
    } else {
        let parent = config.parent().context("Invalid destination config path")?;
        for path in [
            config.to_path_buf(),
            parent.join("lcd_templates.json"),
            parent.join("rgb_presets.json"),
            parent.join("profiles"),
        ] {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => anyhow::bail!(
                    "Fresh destination acquired saved state during migration preparation"
                ),
            }
        }
    }
    Ok(())
}

fn state_fingerprint(files: &[crate::state::StateFile]) -> String {
    let mut files: Vec<_> = files.iter().collect();
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut hash = Sha256::new();
    hash.update(b"lianli-prepared-state-v1\0");
    for file in files {
        let path = file.relative_path.as_os_str().as_encoded_bytes();
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path);
        hash.update((file.bytes.len() as u64).to_le_bytes());
        hash.update(&file.bytes);
    }
    format!("{:x}", hash.finalize())
}

/// The caller must hold the service operation and hardware reservations, stop the
/// destination daemon and execute as its account until publication finishes.
/// The receipt must come from the completed, authenticated preparation.
pub fn begin_publication(
    expected: &PreparedTransfer,
) -> Result<crate::state_transaction::StateTransaction> {
    use crate::media_publication::{Directory, MediaPublication};
    validate_preparation(expected, &expected.config_path, &expected.id)?;
    ensure!(
        expected.decode_validated,
        "Prepare the transfer again with destination media validation"
    );
    ensure!(
        expected.destination_uid == unsafe { libc::geteuid() },
        "Publish under the destination account"
    );
    let state_generation = expected
        .state_generation
        .as_ref()
        .context("Prepare the transfer again to record state integrity")?;
    let media_generation = expected
        .media_generation
        .as_ref()
        .context("Prepare the transfer again to record media integrity")?;
    let parent = expected
        .config_path
        .parent()
        .context("Invalid destination config path")?;
    ensure!(
        parent.is_absolute(),
        "Destination config path must be absolute"
    );
    let root = Directory::open(parent)?;
    let stage = root.child(&expected.directory, false)?;
    let state_dir = stage.child("state", false)?;
    let manifest = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(stage.path().join("manifest.json"))?;
    let metadata = manifest.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == expected.destination_uid
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 60 * 1024,
        "Invalid preparation manifest"
    );
    let mut bytes = Vec::new();
    manifest.take(60 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 60 * 1024,
        "Preparation manifest exceeded its limit"
    );
    let actual: PreparedTransfer = serde_json::from_slice(&bytes)?;
    ensure!(
        &actual == expected,
        "Preparation receipt changed. Inspect it before publication"
    );
    let config_name = expected
        .config_path
        .file_name()
        .context("Invalid destination config name")?;
    let state = StateSnapshot::read(
        &state_dir.path().join(config_name),
        &expected.working_directory,
    )?;
    ensure!(
        state.summary.issue_count == 0
            && state.files.len() == expected.state_files
            && state_fingerprint(&state.files) == *state_generation,
        "Prepared state changed. Prepare the transfer again"
    );
    verify_destination_generation(
        &expected.config_path,
        &expected.working_directory,
        expected.destination_generation.as_deref(),
    )?;
    let media = MediaPublication {
        id: expected.id.clone(),
        preparation: expected.directory.clone(),
        media: expected.media_directory.clone(),
        fingerprint: media_generation.clone(),
    };
    let transaction = crate::state_transaction::StateTransaction::prepare_media(
        &expected.config_path,
        &state.files,
        media,
    )?;
    let current = fs::symlink_metadata(parent)?;
    let held = root.0.metadata()?;
    ensure!(
        current.is_dir() && current.dev() == held.dev() && current.ino() == held.ino(),
        "Destination state directory changed before publication"
    );
    verify_destination_generation(
        &expected.config_path,
        &expected.working_directory,
        expected.destination_generation.as_deref(),
    )?;
    let mut marker = serde_json::to_value(expected)?;
    marker["publication_started"] = true.into();
    marker["backup"] = transaction.backup_name().into();
    let mut file = tempfile::NamedTempFile::new_in(stage.path())?;
    file.write_all(&serde_json::to_vec(&marker)?)?;
    file.as_file().sync_all()?;
    file.persist(stage.path().join("manifest.json"))?;
    stage.0.sync_all()?;
    Ok(transaction)
}

fn reply(channel: &Channel) -> Result<Reply> {
    let (reply, descriptor) = channel.receive()?;
    ensure!(
        descriptor.is_none(),
        "Unexpected descriptor in transfer acknowledgement"
    );
    Ok(reply)
}

fn inherited_channel() -> Result<Channel> {
    Channel::new(std::io::stdin().as_fd().try_clone_to_owned()?, TIMEOUT)
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid migration identifier"
    );
    Ok(())
}

pub fn discard_as(account: &Account, config: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    let command = account.control_command(&[
        OsStr::new("discard-transfer"),
        OsStr::new("--config"),
        config.as_os_str(),
        OsStr::new("--operation-id"),
        OsStr::new(id),
    ])?;
    let result = crate::command::run(command, Duration::from_secs(30))?;
    ensure!(
        result.status.success(),
        "Cannot discard prepared transfer: {}",
        result.stderr.trim()
    );
    Ok(())
}

pub fn discard(config: &Path, id: &str) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Discard preparation under its unprivileged destination account"
    );
    discard_at(config, id)
}

fn discard_at(config: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    let parent = config.parent().context("Invalid destination config path")?;
    ensure!(
        parent.is_absolute(),
        "Destination config path must be absolute"
    );
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = parent.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "Discard preparation under its destination account and protected state directory"
    );
    let pinned = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    ensure!(
        fs::symlink_metadata(pinned.join(".lianli-state-transaction.json"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "Recover the pending state transaction before discarding preparation"
    );
    let prefix = format!("{PREFIX}{id}-");
    let mut directories = Vec::new();
    for (index, entry) in fs::read_dir(&pinned)?.enumerate() {
        ensure!(index < 4096, "Destination has too many state entries");
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0,
                "Pending preparation was replaced. Inspect it before discarding"
            );
            let manifest_path = entry.path().join("manifest.json");
            match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(manifest_path)
            {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    ensure!(
                        metadata.is_file()
                            && metadata.len() <= 64 * 1024
                            && metadata.uid() == unsafe { libc::geteuid() }
                            && metadata.mode() & 0o077 == 0,
                        "Invalid preparation manifest. Inspect it before discarding"
                    );
                    let mut data = Vec::new();
                    file.take(64 * 1024 + 1).read_to_end(&mut data)?;
                    ensure!(
                        data.len() <= 64 * 1024,
                        "Preparation manifest grew beyond its limit"
                    );
                    let manifest: PreparedTransfer = serde_json::from_slice(&data).context("Unrecognized preparation manifest. Do not discard a migration that may have entered publication")?;
                    validate_preparation(&manifest, config, id)?;
                    ensure!(
                        manifest.version == 1
                            && manifest.id == id
                            && manifest.config_path == config
                            && manifest.destination_uid == unsafe { libc::geteuid() }
                            && manifest.directory == entry.file_name().to_string_lossy(),
                        "Preparation does not match the requested operation"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Inspecting preparation manifest"),
            }
            directories.push(entry.path());
        }
    }
    ensure!(
        directories.len() <= 1,
        "Ambiguous preparation directories. Inspect them before discarding"
    );
    for directory in directories {
        fs::remove_dir_all(directory)?;
    }
    parent.sync_all()?;
    Ok(())
}

fn validate_config_name(name: &str) -> Result<()> {
    ensure!(
        Path::new(name).file_name() == Some(OsStr::new(name))
            && !matches!(name, "lcd_templates.json" | "rgb_presets.json" | "profiles")
            && !name.is_empty()
            && name.len() <= 255
            && !name.as_bytes().contains(&0),
        "Invalid source configuration filename"
    );
    Ok(())
}

pub(crate) fn sealed_state(bytes: &[u8]) -> Result<File> {
    ensure!(bytes.len() <= MAX_FILE, "State file exceeds 16 MiB");
    let fd = unsafe {
        libc::memfd_create(
            c"lianli-state-transfer".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(
        fd >= 0,
        "Cannot allocate state transfer memory: {}",
        std::io::Error::last_os_error()
    );
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals()) } == 0,
        "Cannot seal transferred state: {}",
        std::io::Error::last_os_error()
    );
    Ok(file)
}

pub(crate) fn read_sealed_state(fd: OwnedFd, bytes: usize, expected: &str) -> Result<Vec<u8>> {
    ensure!(bytes <= MAX_FILE, "State file exceeds 16 MiB");
    let file = File::from(fd);
    let actual_seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(
        actual_seals >= 0 && actual_seals & seals() == seals(),
        "Transferred state is not immutable"
    );
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() == bytes as u64,
        "Invalid transferred state file length"
    );
    let mut data = vec![0; bytes];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut data, 0)?;
    ensure!(
        digest(&data) == expected,
        "Transferred state checksum mismatch"
    );
    let _: serde_json::Value = serde_json::from_slice(&data)?;
    Ok(data)
}

fn seals() -> i32 {
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
pub(crate) use tests::prepared_fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::daemon::FileIdentity;
    use lianli_shared::media_dependencies::AssetAccessReport;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn receive_into(
        channel: &Channel,
        destination: &Destination,
        id: &str,
    ) -> Result<PreparedTransfer> {
        super::receive_into(channel, destination, id, |_| Ok(()))
    }

    fn destination(root: &Path) -> Destination {
        let config = root.join("config.json");
        Destination {
            scope: ServiceScope::User,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            groups_fingerprint: String::new(),
            mount_namespace: FileIdentity {
                device: "fixture".into(),
                inode: "fixture".into(),
            },
            state: config
                .exists()
                .then(|| StateSnapshot::read(&config, root).unwrap().summary),
            config_path: config,
            working_directory: root.into(),
            assets: AssetAccessReport {
                uid: unsafe { libc::geteuid() },
                checked: 0,
                failed: 0,
                issues: Vec::new(),
            },
        }
    }

    fn transfer(
        config: &Path,
        working: &Path,
        target: &Destination,
    ) -> (Result<StateSummary>, Result<PreparedTransfer>) {
        let snapshot = StateSnapshot::read(config, working).unwrap();
        let (sender, receiver) = Channel::pair().unwrap();
        let sender = Channel::new(sender, Duration::from_secs(5)).unwrap();
        let receiver = Channel::new(receiver, Duration::from_secs(5)).unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || send_snapshot(&sender, snapshot, config, working));
            let received = receive_into(&receiver, target, ID);
            drop(receiver);
            (worker.join().unwrap(), received)
        })
    }

    pub(crate) fn prepared_fixture() -> (tempfile::TempDir, tempfile::TempDir, PreparedTransfer) {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(source.path().join("image.png"), b"fixture media").unwrap();
        let config = source.path().join("config.json");
        fs::write(
            &config,
            br#"{"lcds":[{"index":0,"type":"image","path":"image.png"}],"future":42}"#,
        )
        .unwrap();
        fs::write(target.path().join("config.json"), b"{\"previous\":true}").unwrap();
        let (sent, received) = transfer(&config, source.path(), &destination(target.path()));
        sent.unwrap();
        (source, target, received.unwrap())
    }

    #[test]
    fn transfers_sensor_fonts_before_the_final_media_directory_exists() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let font = source.path().join("sensor.ttf");
        fs::write(&font, b"font fixture").unwrap();
        let config = source.path().join("config.json");
        fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "lcds": [{"index": 0, "type": "sensor", "sensor": {
                    "label": "Load", "unit": "%", "source": {"type": "constant", "value": 50},
                    "font_path": font
                }}]
            }))
            .unwrap(),
        )
        .unwrap();
        let (sent, received) = transfer(&config, source.path(), &destination(target.path()));
        sent.unwrap();
        let prepared = received.unwrap();
        assert!(!prepared.final_media_path.exists());
        let transaction = begin_publication(&prepared).unwrap();
        transaction.publish().unwrap();
        let published: lianli_shared::config::AppConfig =
            serde_json::from_slice(&fs::read(&prepared.config_path).unwrap()).unwrap();
        let path = published.lcds[0]
            .sensor
            .as_ref()
            .unwrap()
            .font_path
            .as_ref()
            .unwrap();
        assert!(path.starts_with(&prepared.final_media_path));
        assert_eq!(fs::read(path).unwrap(), b"font fixture");
        published.lcds[0].validate().unwrap();
    }

    #[test]
    fn publishes_prepared_media_before_state_and_retains_imports_for_restore_backups() {
        let (source, target, prepared) = prepared_fixture();
        let original = fs::read(source.path().join("config.json")).unwrap();
        let transaction = begin_publication(&prepared).unwrap();
        assert!(discard_at(&prepared.config_path, ID).is_err());
        assert!(!prepared.final_media_path.exists());
        let backup = transaction.publish().unwrap();
        let published: serde_json::Value =
            serde_json::from_slice(&fs::read(&prepared.config_path).unwrap()).unwrap();
        let asset = Path::new(published["lcds"][0]["path"].as_str().unwrap());
        assert_eq!(fs::read(asset).unwrap(), b"fixture media");
        assert!(asset.starts_with(&prepared.final_media_path));
        assert_eq!(published["future"], 42);
        assert!(!target
            .path()
            .join(&prepared.directory)
            .join(&prepared.media_directory)
            .exists());
        assert!(discard_at(&prepared.config_path, ID).is_err());
        let undo =
            crate::state_transaction::restore_backup(&prepared.config_path, &backup).unwrap();
        assert_eq!(
            fs::read(&prepared.config_path).unwrap(),
            b"{\"previous\":true}"
        );
        assert_eq!(fs::read(asset).unwrap(), b"fixture media");
        crate::state_transaction::restore_backup(&prepared.config_path, &undo).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&prepared.config_path).unwrap())
                .unwrap(),
            published
        );
        assert_eq!(
            fs::read(source.path().join("config.json")).unwrap(),
            original
        );
    }

    #[test]
    fn refuses_changed_preparations_and_destinations_before_creating_a_journal() {
        for change in ["state", "media", "manifest", "destination", "legacy"] {
            let (_source, target, mut prepared) = prepared_fixture();
            let stage = target.path().join(&prepared.directory);
            match change {
                "state" => fs::write(stage.join("state/config.json"), b"{}").unwrap(),
                "media" => {
                    let object = fs::read_dir(stage.join(&prepared.media_directory))
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path();
                    fs::write(object, b"changed media").unwrap();
                }
                "manifest" => {
                    let mut other = prepared.clone();
                    other.source_generation = "0".repeat(64);
                    fs::write(
                        stage.join("manifest.json"),
                        serde_json::to_vec(&other).unwrap(),
                    )
                    .unwrap();
                }
                "destination" => fs::write(&prepared.config_path, b"{\"external\":true}").unwrap(),
                "legacy" => prepared.media_generation = None,
                _ => unreachable!(),
            }
            assert!(begin_publication(&prepared).is_err(), "{change}");
            assert!(
                !target
                    .path()
                    .join(".lianli-state-transaction.json")
                    .exists(),
                "{change}"
            );
            assert!(!prepared.final_media_path.exists(), "{change}");
            let expected: &[u8] = if change == "destination" {
                b"{\"external\":true}"
            } else {
                b"{\"previous\":true}"
            };
            assert_eq!(fs::read(&prepared.config_path).unwrap(), expected);
        }
    }

    #[test]
    fn publication_supports_fresh_destinations_and_empty_media() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let config = source.path().join("config.json");
        fs::write(&config, b"{\"future\":true}").unwrap();
        let (sent, received) = transfer(&config, source.path(), &destination(target.path()));
        sent.unwrap();
        let prepared = received.unwrap();
        begin_publication(&prepared).unwrap().publish().unwrap();
        assert_eq!(fs::read_dir(&prepared.final_media_path).unwrap().count(), 0);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&prepared.config_path).unwrap())
                .unwrap()["future"],
            true
        );
    }

    #[test]
    fn destination_decode_failure_checks_staged_children_and_discards_the_transfer() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let config = source.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        fs::write(source.path().join("child.gif"), b"invalid gif").unwrap();
        fs::write(source.path().join("lcd_templates.json"), br#"{"templates":[{"id":"unused","name":"Unused","base_width":400,"base_height":400,"background":{"type":"color","rgb":[0,0,0]},"widgets":[{"id":"child","x":0,"y":0,"width":10,"height":10,"kind":{"type":"video","path":"child.gif"}}]}]}"#).unwrap();
        let destination = destination(target.path());
        let snapshot = StateSnapshot::read(&config, source.path()).unwrap();
        let (sender, receiver) = Channel::pair().unwrap();
        let sender = Channel::new(sender, Duration::from_secs(5)).unwrap();
        let receiver = Channel::new(receiver, Duration::from_secs(5)).unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| send_snapshot(&sender, snapshot, &config, source.path()));
            let received = super::receive_into(&receiver, &destination, ID, |dependencies| {
                assert_eq!(dependencies.len(), 1);
                assert!(dependencies[0].owner.contains("child"));
                assert_eq!(fs::read(&dependencies[0].path).unwrap(), b"invalid gif");
                assert_ne!(dependencies[0].path, source.path().join("child.gif"));
                anyhow::bail!("fixture codec failure")
            });
            assert!(received.unwrap_err().to_string().contains("codec failure"));
            drop(receiver);
            assert!(worker.join().unwrap().is_err());
        });
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
        assert_eq!(fs::read(config).unwrap(), b"{}");
    }

    #[test]
    fn account_helpers_transfer_over_inherited_descriptors_without_a_shared_source_directory() {
        if let Ok(role) = std::env::var("LIANLI_TRANSFER_FIXTURE_ROLE") {
            let source = PathBuf::from(std::env::var_os("LIANLI_TRANSFER_FIXTURE_SOURCE").unwrap());
            let target = PathBuf::from(std::env::var_os("LIANLI_TRANSFER_FIXTURE_TARGET").unwrap());
            if role == "source" {
                send(
                    &source.join("config.json"),
                    &source,
                    &std::env::var("LIANLI_TRANSFER_FIXTURE_GENERATION").unwrap(),
                )
                .unwrap();
            } else {
                assert_eq!(role, "destination");
                if std::env::var_os("LIANLI_TRANSFER_FIXTURE_DISTINCT_UIDS").is_some() {
                    assert!(File::open(source.join("config.json")).is_err());
                }
                receive_into(&inherited_channel().unwrap(), &destination(&target), ID).unwrap();
            }
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("destination");
        fs::DirBuilder::new().mode(0o700).create(&source).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        fs::write(
            source.join("config.json"),
            br#"{"lcds":[{"index":0,"type":"image","path":"private.png"}]}"#,
        )
        .unwrap();
        fs::write(source.join("private.png"), b"private fixture pixels").unwrap();
        let privileged = unsafe { libc::geteuid() } == 0;
        let account = |uid| {
            let gid = if privileged {
                uid
            } else {
                unsafe { libc::getegid() }
            };
            let mut groups = if privileged {
                Vec::new()
            } else {
                let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
                assert!(count >= 0);
                let mut groups = vec![0; count as usize];
                assert_eq!(
                    unsafe { libc::getgroups(count, groups.as_mut_ptr()) },
                    count
                );
                groups
            };
            groups.push(gid);
            groups.sort_unstable();
            groups.dedup();
            Account {
                uid,
                gid,
                groups,
                name: "fixture".into(),
                home: root.path().into(),
            }
        };
        let source_account = account(if privileged {
            65534
        } else {
            unsafe { libc::geteuid() }
        });
        let destination_account = account(if privileged {
            65533
        } else {
            unsafe { libc::geteuid() }
        });
        if privileged {
            for (path, account) in [(&source, &source_account), (&target, &destination_account)] {
                let directory = File::open(path).unwrap();
                assert_eq!(
                    unsafe { libc::fchown(directory.as_raw_fd(), account.uid, account.gid) },
                    0
                );
            }
        }
        let generation = StateSnapshot::read(&source.join("config.json"), &source)
            .unwrap()
            .summary
            .generation;
        let command = |account: &Account, role: &str| {
            let mut command = account.control_command(&[
                OsStr::new("--exact"), OsStr::new("state_transfer::tests::account_helpers_transfer_over_inherited_descriptors_without_a_shared_source_directory"),
            ]).unwrap();
            command
                .env("LIANLI_TRANSFER_FIXTURE_ROLE", role)
                .env("LIANLI_TRANSFER_FIXTURE_SOURCE", &source)
                .env("LIANLI_TRANSFER_FIXTURE_TARGET", &target)
                .env("LIANLI_TRANSFER_FIXTURE_GENERATION", &generation);
            if privileged {
                command.env("LIANLI_TRANSFER_FIXTURE_DISTINCT_UIDS", "1");
            }
            command
        };
        let source_command = command(&source_account, "source");
        let target_command = command(&destination_account, "destination");
        let (left, right) = Channel::pair().unwrap();
        let (sent, received) = std::thread::scope(|scope| {
            let source = scope.spawn(move || {
                crate::command::run_with_stdin(
                    source_command,
                    Stdio::from(left),
                    Duration::from_secs(10),
                )
            });
            let received = crate::command::run_with_stdin(
                target_command,
                Stdio::from(right),
                Duration::from_secs(10),
            );
            (source.join().unwrap().unwrap(), received.unwrap())
        });
        assert!(sent.status.success(), "{} {}", sent.stdout, sent.stderr);
        assert!(
            received.status.success(),
            "{} {}",
            received.stdout,
            received.stderr
        );
        let stage = fs::read_dir(&target)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::metadata(&stage).unwrap().uid(), destination_account.uid);
        let manifest: PreparedTransfer =
            serde_json::from_slice(&fs::read(stage.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.source_generation, generation);
        assert_eq!(manifest.destination_uid, destination_account.uid);
        assert!(!target.join("config.json").exists());
        assert_eq!(
            fs::read(source.join("private.png")).unwrap(),
            b"private fixture pixels"
        );
    }

    #[test]
    fn transfers_complete_saved_state_and_media_without_publishing_or_reopening_source_paths() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let config = source.path().join("custom.json");
        fs::write(&config, br#"{"lcds":[{"index":0,"type":"image","path":"active.png"}],"future":{"preserve":17}}"#).unwrap();
        fs::write(source.path().join("active.png"), b"fixture pixels").unwrap();
        fs::write(source.path().join("child.jpg"), b"fixture pixels").unwrap();
        fs::write(source.path().join("lcd_templates.json"), br#"{"templates":[{"id":"unused","name":"Unused","base_width":400,"base_height":400,"background":{"type":"image","path":"child.jpg"},"widgets":[]}],"future":true}"#).unwrap();
        fs::write(source.path().join("rgb_presets.json"), b"[]").unwrap();
        fs::create_dir(source.path().join("profiles")).unwrap();
        fs::write(source.path().join("profiles/Inactive.json"), br#"{"name":"Inactive","device_id":"offline","future":41,"lcds":[{"index":0,"type":"image","path":"active.png"}]}"#).unwrap();
        fs::write(target.path().join("config.json"), b"{\"keep\":true}").unwrap();
        let before = fs::read(&config).unwrap();
        let target_state = destination(target.path());
        let (sent, received) = transfer(&config, source.path(), &target_state);
        let received = received.unwrap();
        assert_eq!(sent.unwrap().generation, received.source_generation);
        assert_eq!(received.state_files, 4);
        assert_eq!(received.media_files, 1);
        assert_eq!(received.media_bytes, 14);
        assert_eq!(fs::read(&config).unwrap(), before);
        assert_eq!(
            fs::read(target.path().join("config.json")).unwrap(),
            b"{\"keep\":true}"
        );
        assert!(!received.final_media_path.exists());
        let stage = target.path().join(&received.directory);
        assert_eq!(fs::metadata(&stage).unwrap().mode() & 0o777, 0o700);
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(stage.join("state/config.json")).unwrap()).unwrap();
        assert_eq!(rewritten["future"]["preserve"], 17);
        let mapped = Path::new(rewritten["lcds"][0]["path"].as_str().unwrap());
        assert!(mapped.starts_with(&received.final_media_path));
        let pixels = stage
            .join(&received.media_directory)
            .join(mapped.file_name().unwrap());
        assert_eq!(fs::read(&pixels).unwrap(), b"fixture pixels");
        assert_eq!(fs::metadata(&pixels).unwrap().uid(), unsafe {
            libc::geteuid()
        });
        assert_eq!(fs::metadata(&pixels).unwrap().mode() & 0o777, 0o600);
        let profile: serde_json::Value =
            serde_json::from_slice(&fs::read(stage.join("state/profiles/Inactive.json")).unwrap())
                .unwrap();
        assert_eq!(profile["future"], 41);
        assert_eq!(
            fs::read(stage.join("state/rgb_presets.json")).unwrap(),
            b"[]"
        );
        let another_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        discard_at(&target_state.config_path, another_id).unwrap();
        assert!(stage.exists());
        discard_at(&target_state.config_path, ID).unwrap();
        assert!(!stage.exists());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    fn destination_copies_an_unlinked_descriptor_and_rejects_mutable_state() {
        let root = tempfile::tempdir().unwrap();
        let target = destination(root.path());
        let (sender, receiver) = Channel::pair().unwrap();
        let sender = Channel::new(sender, Duration::from_secs(5)).unwrap();
        let receiver = Channel::new(receiver, Duration::from_secs(5)).unwrap();
        let input = tempfile::NamedTempFile::new().unwrap();
        fs::write(input.path(), b"descriptor only").unwrap();
        let file = File::open(input.path()).unwrap();
        input.close().unwrap();
        std::thread::scope(|scope| {
            let child = scope.spawn(move || {
                sender
                    .send(
                        &Message::Begin {
                            generation: "a".repeat(64),
                            config_name: "config.json".into(),
                            assets: 1,
                            files: 1,
                        },
                        None,
                    )
                    .unwrap();
                assert!(matches!(reply(&sender).unwrap(), Reply::Ready));
                sender
                    .send(
                        &Message::Asset {
                            path: "/not-visible-to-destination/private.png".into(),
                        },
                        Some(file.as_fd()),
                    )
                    .unwrap();
                let Reply::Copied(path) = reply(&sender).unwrap() else {
                    panic!()
                };
                assert!(path.ends_with(format!("{}.png", digest(b"descriptor only"))));
                let mutable = tempfile::tempfile().unwrap();
                sender
                    .send(
                        &Message::State {
                            path: "config.json".into(),
                            bytes: 0,
                            digest: digest(b""),
                        },
                        Some(mutable.as_fd()),
                    )
                    .unwrap();
            });
            let error = receive_into(&receiver, &target, ID).unwrap_err();
            assert!(error.to_string().contains("immutable"));
            child.join().unwrap();
        });
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn rejects_changed_destination_and_cleans_uncommitted_transfer() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let config = source.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        fs::write(target.path().join("config.json"), b"{}").unwrap();
        let target_state = destination(target.path());
        fs::write(&target_state.config_path, b"{\"external\":true}").unwrap();
        let (sent, received) = transfer(&config, source.path(), &target_state);
        assert!(sent.is_err());
        assert!(received
            .unwrap_err()
            .to_string()
            .contains("Destination state changed"));
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
        assert_eq!(
            fs::read(&target_state.config_path).unwrap(),
            b"{\"external\":true}"
        );
    }

    #[test]
    fn source_changes_while_destination_finishes_preparation_prevent_acceptance() {
        let source = tempfile::tempdir().unwrap();
        let config = source.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let snapshot = StateSnapshot::read(&config, source.path()).unwrap();
        let (sender, receiver) = Channel::pair().unwrap();
        let sender = Channel::new(sender, Duration::from_secs(5)).unwrap();
        let receiver = Channel::new(receiver, Duration::from_secs(5)).unwrap();
        let result = std::thread::scope(|scope| {
            let worker = scope.spawn(|| send_snapshot(&sender, snapshot, &config, source.path()));
            let (begin, descriptor): (Message, _) = receiver.receive().unwrap();
            assert!(matches!(
                begin,
                Message::Begin {
                    assets: 0,
                    files: 1,
                    ..
                }
            ));
            assert!(descriptor.is_none());
            receiver.send(&Reply::Ready, None).unwrap();
            let (state, descriptor): (Message, _) = receiver.receive().unwrap();
            assert!(matches!(state, Message::State { .. }));
            drop(descriptor);
            receiver.send(&Reply::StateAccepted, None).unwrap();
            let (finish, _): (Message, _) = receiver.receive().unwrap();
            assert!(matches!(finish, Message::Finish));
            fs::write(&config, b"{\"external\":true}").unwrap();
            receiver.send(&Reply::Prepared, None).unwrap();
            worker.join().unwrap()
        });
        assert!(result.unwrap_err().to_string().contains("State changed"));
        assert_eq!(fs::read(&config).unwrap(), b"{\"external\":true}");
    }

    #[test]
    fn destination_changes_before_final_acceptance_discard_only_the_preparation() {
        let root = tempfile::tempdir().unwrap();
        let target = destination(root.path());
        let (sender, receiver) = Channel::pair().unwrap();
        let sender = Channel::new(sender, Duration::from_secs(5)).unwrap();
        let receiver = Channel::new(receiver, Duration::from_secs(5)).unwrap();
        let result = std::thread::scope(|scope| {
            let worker = scope.spawn(|| receive_into(&receiver, &target, ID));
            sender
                .send(
                    &Message::Begin {
                        generation: "a".repeat(64),
                        config_name: "config.json".into(),
                        assets: 0,
                        files: 1,
                    },
                    None,
                )
                .unwrap();
            assert!(matches!(reply(&sender).unwrap(), Reply::Ready));
            let data = sealed_state(b"{}").unwrap();
            sender
                .send(
                    &Message::State {
                        path: "config.json".into(),
                        bytes: 2,
                        digest: digest(b"{}"),
                    },
                    Some(data.as_fd()),
                )
                .unwrap();
            assert!(matches!(reply(&sender).unwrap(), Reply::StateAccepted));
            sender.send(&Message::Finish, None).unwrap();
            assert!(matches!(reply(&sender).unwrap(), Reply::Prepared));
            fs::write(&target.config_path, b"{\"external\":true}").unwrap();
            sender.send(&Message::Accepted, None).unwrap();
            worker.join().unwrap()
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("acquired saved state"));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(
            fs::read(&target.config_path).unwrap(),
            b"{\"external\":true}"
        );
    }

    #[test]
    fn reports_the_originating_helper_failure_alongside_a_peer_disconnect() {
        use std::os::unix::process::ExitStatusExt;
        let failed = |message: &str| {
            Ok(crate::command::Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: String::new(),
                stderr: message.into(),
            })
        };
        let error = successful_outputs(
            failed("Source asset lost access"),
            failed("State transfer peer disconnected"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Source asset lost access"));
        assert!(error.contains("Destination transfer failed"));
        let error = successful_outputs(
            failed("State transfer peer disconnected"),
            failed("Destination disk is full"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Destination disk is full"));
    }

    #[test]
    fn pending_preparation_blocks_more_copying_and_discard_refuses_foreign_or_published_records() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let config = source.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        let target_state = destination(target.path());
        let (_, first) = transfer(&config, source.path(), &target_state);
        let first = first.unwrap();
        validate_preparation(&first, &target_state.config_path, ID).unwrap();
        let mut invalid: PreparedTransfer =
            serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
        invalid.media_directory = "../../outside".into();
        assert!(validate_preparation(&invalid, &target_state.config_path, ID).is_err());
        let (sent, received) = transfer(&config, source.path(), &target_state);
        assert!(sent.is_err());
        assert!(received
            .unwrap_err()
            .to_string()
            .contains("pending migration"));
        let manifest = target.path().join(&first.directory).join("manifest.json");
        let mut document = serde_json::to_value(&first).unwrap();
        document["publication_started"] = true.into();
        fs::write(&manifest, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(discard_at(&target_state.config_path, ID).is_err());
        assert!(manifest.exists());
        document
            .as_object_mut()
            .unwrap()
            .remove("publication_started");
        document["destination_uid"] = 4294967295u32.into();
        fs::write(&manifest, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(discard_at(&target_state.config_path, ID).is_err());
    }

    #[test]
    fn immutable_state_checks_size_hash_and_unsafe_paths_before_acceptance() {
        let file = sealed_state(b"{}").unwrap();
        assert!(read_sealed_state(
            file.as_fd().try_clone_to_owned().unwrap(),
            3,
            &digest(b"{}")
        )
        .is_err());
        assert!(read_sealed_state(file.as_fd().try_clone_to_owned().unwrap(), 2, "wrong").is_err());
        assert_eq!(
            read_sealed_state(
                file.as_fd().try_clone_to_owned().unwrap(),
                2,
                &digest(b"{}")
            )
            .unwrap(),
            b"{}"
        );
        for name in [
            "../config.json",
            "profiles",
            "lcd_templates.json",
            "./config",
            "",
            "/etc/passwd",
            "bad\0name",
        ] {
            assert!(validate_config_name(name).is_err(), "{name:?}");
        }
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"untouched").unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            root.path().join(format!("{PREFIX}{ID}-alias")),
        )
        .unwrap();
        assert!(discard_at(&root.path().join("config.json"), ID).is_err());
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"untouched");
    }
}
