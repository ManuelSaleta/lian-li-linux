use anyhow::{ensure, Context, Result};
use lianli_shared::media_dependencies::AssetKind;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::process::CommandExt;

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum Kind {
    File,
    Image,
    Video,
    Gif,
    Font,
}

pub fn run(kind: Kind, extension: Option<&str>) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Decode media under the unprivileged destination account"
    );
    let file = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 2 * 1024 * 1024 * 1024,
        "Media decode input must be a regular file of at most 2 GiB"
    );
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY && flags & libc::O_PATH == 0,
        "Media decode input must be read-only"
    );
    for (resource, ceiling) in [
        (libc::RLIMIT_AS, 1024 * 1024 * 1024),
        (libc::RLIMIT_CPU, 20),
        (libc::RLIMIT_FSIZE, 0),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        ensure!(
            unsafe { libc::getrlimit(resource, &mut limits) } == 0,
            "Cannot inspect media helper limits"
        );
        limits.rlim_cur = limits.rlim_cur.min(ceiling);
        limits.rlim_max = limits.rlim_max.min(ceiling);
        ensure!(
            unsafe { libc::setrlimit(resource, &limits) } == 0,
            "Cannot bound media helper resources"
        );
    }
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0,
        "Cannot constrain media helper privileges"
    );
    let kind = match kind {
        Kind::File => AssetKind::File,
        Kind::Image => AssetKind::Image,
        Kind::Font => AssetKind::Font,
        Kind::Video | Kind::Gif => {
            if matches!(kind, Kind::Gif) {
                lianli_media::validation::check_still(file, AssetKind::Gif, None)?;
            }
            // Exec retains the supervisor's process identity, deadline and parent-death signal.
            return Err(lianli_media::validation::video_command().exec())
                .context("Starting software video validation");
        }
    };
    lianli_media::validation::check_still(file, kind, extension)
}
