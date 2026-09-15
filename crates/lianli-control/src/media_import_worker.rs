use crate::media_import::PublishedSelection;
use crate::media_import_job::{self, Job, Status};
use anyhow::{ensure, Context, Result};
use lianli_shared::{installation::InstallationContext, services::ServiceScope};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn start(
    scope: ServiceScope,
    lcds: Vec<lianli_shared::config::LcdConfig>,
    templates: Vec<lianli_shared::template::LcdTemplate>,
    config: &Path,
) -> Result<String> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Start managed imports from the native application"
    );
    ensure!(
        !read()?.is_some_and(|status| status.active),
        "A managed import is already active"
    );
    crate::media_import::selection_dependencies(&lcds, &templates)?;
    let path = runtime_path()?;
    match fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let directory = crate::media_publication::Directory::open(&path)?;
    ensure!(
        directory.0.metadata()?.mode() & 0o077 == 0,
        "Import request directory must be private"
    );
    for (index, entry) in fs::read_dir(directory.path())?.enumerate() {
        entry?;
        ensure!(
            index < 33,
            "Too many retained import request files; inspect pending imports before retrying"
        );
    }
    let mut random = [0; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let name = format!("selection-{id}.json");
    let selection = path.join(&name);
    let arguments = launch_arguments(scope, &selection, config, &id)?;
    let mut request = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.path().join(&name))?;
    request.write_all(&serde_json::to_vec(&(lcds, templates))?)?;
    request.sync_all()?;
    directory.0.sync_all()?;
    let mut command = Command::new("/usr/bin/systemd-run");
    command.args(arguments);
    let output = crate::command::run(command, Duration::from_secs(10))
        .context("Import submission was not confirmed; check progress before retrying. The request was retained")?;
    ensure!(
        output.status.success(),
        "Cannot submit import worker: {}. Check progress before retrying; the request was retained",
        output.stderr.chars().take(2048).collect::<String>()
    );
    Ok(id)
}

fn launch_arguments(
    scope: ServiceScope,
    selection: &Path,
    config: &Path,
    id: &str,
) -> Result<Vec<String>> {
    let command = authorization_command(scope, selection, config, id)?;
    let mut args: Vec<String> = [
        "--user",
        "--quiet",
        "--collect",
        "--no-ask-password",
        "--unit=lianli-media-import.service",
        "--property=Type=exec",
        "--property=Restart=no",
        "--property=KillMode=control-group",
        "--property=RuntimeMaxSec=840s",
        "--property=TimeoutStopSec=10s",
        "--property=StandardInput=null",
        "--property=StandardOutput=null",
        "--property=StandardError=journal",
        "--",
        "/usr/bin/lianli-control",
        "run-selected-media-import",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for argument in command.get_args().skip(3) {
        args.push(
            argument
                .to_str()
                .context("Import paths must be UTF-8")?
                .replace('$', "$$")
                .replace('%', "%%"),
        );
    }
    Ok(args)
}

pub fn runtime_path() -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    ensure!(
        uid != 0,
        "Managed import progress belongs to the desktop user"
    );
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let metadata = fs::symlink_metadata(&runtime)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "A private desktop-user runtime directory is required for managed imports"
    );
    Ok(runtime.join("lianli-media-import"))
}

pub fn read() -> Result<Option<Status>> {
    media_import_job::read(&runtime_path()?)
}

pub fn run(scope: ServiceScope, selection: &Path, expected_config: &Path, id: &str) -> Result<()> {
    ensure!(
        InstallationContext::detect() == InstallationContext::Native,
        "Run managed import workers in the native desktop session"
    );
    let job = Job::begin(&runtime_path()?, id)?;
    let result = (|| -> Result<PublishedSelection> {
        let command = authorization_command(scope, selection, expected_config, id)?;
        let output = crate::command::run_with_stdin_limit(
            command,
            Stdio::null(),
            Duration::from_secs(780),
            16 * 1024 * 1024,
        )
        .context(
            "Import authorization or execution did not finish; inspect storage before retrying",
        )?;
        ensure!(output.status.success(), "Managed import failed: {}. Check the desktop authentication agent and installed helper, then inspect storage before retrying",
            output.stderr.chars().take(2048).collect::<String>());
        let result: PublishedSelection = serde_json::from_str(&output.stdout).context(
            "Managed import returned an invalid result; inspect storage before retrying",
        )?;
        crate::media_import_launch::validate_result(&result, expected_config, id)?;
        Ok(result)
    })();
    job.finish(result)?;
    let runtime = runtime_path()?;
    let name = format!("selection-{id}.json");
    if selection == runtime.join(&name) {
        let directory = crate::media_publication::Directory::open(&runtime)?;
        let metadata = fs::symlink_metadata(directory.path().join(&name))?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Import outcome was saved, but its request file changed; preserve it for inspection"
        );
        fs::remove_file(directory.path().join(name))?;
        directory.0.sync_all()?;
    }
    Ok(())
}

fn authorization_command(
    scope: ServiceScope,
    selection: &Path,
    config: &Path,
    id: &str,
) -> Result<Command> {
    ensure!(
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid import identity"
    );
    ensure!(
        selection.is_absolute()
            && config.is_absolute()
            && selection.as_os_str().len() <= 4096
            && config.as_os_str().len() <= 4096,
        "Managed import paths must be absolute and at most 4096 bytes"
    );
    let mut command = Command::new("/usr/bin/pkexec");
    command
        .args([
            "--disable-internal-agent",
            "/usr/bin/lianli-control",
            "import-selected-media",
            "--scope",
            match scope {
                ServiceScope::User => "user",
                ServiceScope::System => "system",
            },
            "--selection",
        ])
        .arg(selection)
        .arg("--expected-config")
        .arg(config)
        .arg("--operation-id")
        .arg(id);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_submission_bounds_lifetime_and_escapes_systemd_expansions() {
        let args = launch_arguments(
            ServiceScope::User,
            Path::new("/tmp/$request%u.json"),
            Path::new("/home/user/config.json"),
            "1234567890abcdef1234567890abcdef",
        )
        .unwrap();
        assert!(args
            .iter()
            .any(|arg| arg == "--property=RuntimeMaxSec=840s"));
        assert!(args
            .iter()
            .any(|arg| arg == "--property=KillMode=control-group"));
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            &args[separator + 1..separator + 4],
            &[
                "/usr/bin/lianli-control",
                "run-selected-media-import",
                "--scope"
            ]
        );
        assert!(args.iter().any(|arg| arg == "/tmp/$$request%%u.json"));
        assert!(!args.iter().any(|arg| arg == "/bin/sh"));
    }
    #[test]
    fn authorization_keeps_paths_as_literal_arguments_and_rejects_relative_targets() {
        let selection = Path::new("/tmp/selection $name; file.json");
        let config = Path::new("/home/user space/config.json");
        let command = authorization_command(
            ServiceScope::System,
            selection,
            config,
            "1234567890abcdef1234567890abcdef",
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(command.get_program(), "/usr/bin/pkexec");
        assert_eq!(args[6], selection.as_os_str());
        assert_eq!(args[8], config.as_os_str());
        assert!(authorization_command(
            ServiceScope::User,
            Path::new("relative"),
            config,
            "1234567890abcdef1234567890abcdef"
        )
        .is_err());
    }
}
