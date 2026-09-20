# Service modes and daemon ownership

Run one hardware daemon at a time. The user service runs in your login session; the system
service runs as the `lianli` account. Both use the same host lock at
`/run/lianli-daemon.lock`. A missing or inaccessible lock now prevents startup instead of
allowing separate hardware owners.

Settings and Installation Health show read-only snapshots of both host units,
their effective startup state and global user-service enablement. Recheck after
external changes. Discovery reads the host lock holder's process start time,
effective UID and systemd control group. It matches that group with the queried
user/system units, including child groups used by launch wrappers. Container PIDs
are never compared with host PIDs. Unrelated/manual owners remain distinct;
missing process visibility or changing ownership is reported as unverified.
Settings provides Start, Stop and Restart for native and managed Distrobox services.
These actions retain startup settings. The mode selector switches exclusive startup selection and offers either
copying current settings/media or using the destination account's saved settings.
Save or discard pending edits and close the template editor before switching.

On a fresh native install, the setup popup offers User (at login) and System (at
boot). Choose one and select **Enable and start**. The same control is available
in Settings. After authorization, `lianli-control` enables only the chosen mode,
starts it and verifies the daemon. The destination's saved settings are used if
present, otherwise its defaults apply. The first-run shortcut requires both units
to be installed, inactive and disabled, with no selected mode or hardware owner.
Conflicting or unverified state must be resolved through the normal service controls.

## Service operation lifetime

Native mode switches and the Recover switch action use a separate system service,
`lianli-control-switch.service`, submitted through `pkexec`. Install the native
root-owned `/usr/bin/lianli-control` and your distribution's Polkit/pkexec package,
and run a desktop authentication agent. A binary in a user-writable release
directory cannot be submitted as the system worker. Authorization cancellation
does not submit a switch; ambiguous submission errors require rechecking progress.
The package is named `pkexec` on [Debian](https://packages.debian.org/trixie/pkexec)
and [Ubuntu](https://packages.ubuntu.com/noble/pkexec), and is supplied by `polkit`
on [Arch](https://archlinux.org/packages/extra/x86_64/polkit/files/) and
[Fedora](https://packages.fedoraproject.org/pkgs/polkit/polkit/fedora-44.html).

The system manager owns this worker independently of the GUI and user manager.
Closing the GUI leaves an accepted switch running. Loss of the user's manager or
home access can still prevent account verification or recovery; the switch journal
is preserved in that case. Recovery is requested again at boot, graphical login
and native GUI launch. A previous system service can be restored while the user's
manager or home is unavailable. User-state cleanup remains pending until access
returns. Reopen Settings and use Recover switch if an automatic attempt cannot finish.

One bounded root-owned progress record in `/run/lianli-switch` reports the current
stage and result to reopened GUIs. It contains the caller UID and requested mode,
but excludes private paths and detailed failure output. Details are available with:

```sh
journalctl -u lianli-control-switch.service -b --no-pager
```

Switch workers have no automatic restart. A missing worker is reported as
interrupted, without replaying a request. Recovery uses the durable switch journal;
the runtime progress record is only a display aid and disappears on reboot.

### Automatic recovery attempts

Packages install `lianli-control-recovery.service` as a conditional boot dependency,
plus a graphical-login trigger. The service runs only when the private root switch
journal exists. The native GUI also requests this service once at launch.
Concurrent attempts use the same progress and operation locks as explicit recovery.

When a boot recovery is already running, a login or GUI trigger observes that
invocation until it finishes before submitting its request. This gives recovery
another opportunity to use the newly available user session and home directory.
If a newer invocation has already started, the trigger leaves it alone. Observation
runs every five seconds for at most fifteen minutes, only while handling that
trigger. An observation or submission failure is reported without resubmitting.
There is no permanent recovery watcher or service restart timer. Invocation IDs
identify individual systemd [runtime cycles](https://github.com/systemd/systemd/blob/v255/man/systemd.exec.xml).

An active local user may start only this fixed recovery unit without a new prompt.
The packaged Polkit rule requires both the exact unit and the `start` verb. Other
service actions, unit changes and requests without those details retain their
normal authorization. This uses systemd's
[per-unit authorization details](https://github.com/systemd/systemd/blob/v255/src/core/dbus-util.c);
[transient-unit creation](https://github.com/systemd/systemd/blob/v255/src/core/dbus-manager.c)
uses the separate check without those details. The helper takes no caller-supplied
mode, account or file path: it resumes the account and transaction already recorded
by the authorized switch. It rechecks the journal identity before recovery, and
does nothing when no journal exists. A changed account or journal requires recheck.

After reboot, recovery starts the previous mode only if that mode was persistently
enabled. A manually started or runtime-enabled process from the previous boot is
not recreated. Within the original boot and user runtime directory, recovery
retains the recorded running state. If logout removes that directory, runtime-only
user enablement expires, and a user source is restarted only if it was persistently
enabled. User-manager lingering that retains the runtime directory retains its
runtime-only selection. System runtime enablement is unaffected by user logout.
The journal records the directory's device/inode identity and checks it during
preparation and recovery; replacement during restoration leaves recovery pending
for the current session. Startup selection is checked again before source restart.
Older journals without boot identity, or without runtime identity when user
recovery is ambiguous, require administrator inspection.

If the previous mode was the system service, recovery can restore it before the
caller's user manager or home directory is available. It verifies the system
account, installed unit, configuration and hardware owner, restores system startup
policy and selects only the system mode. The user destination remains blocked;
its files and enablement are restored when the user session and home are available.
A previously disabled system source remains stopped after reboot.

Deferred cleanup first checks access under the destination account. It then briefly
stops the restored system daemon cleanly, restores the user destination while both
daemons are stopped, and restores the original startup selection and running state.
An unavailable home therefore does not repeatedly stop a working system source.
The journal remains pending until this cleanup and verification finish.

Offline system startup is recorded before submission. An uncertain submission is
not automatically repeated during the same boot; inspect the system service and
recovery journals before requesting another recovery. Startup and IPC verification
wait for a bounded interval without forcing shutdown. If the original system source
is already running behind this switch's paused gate, recovery authenticates its
account, hardware ownership, loaded configuration and startup policy before and
after reopening startup for only that source. It need not stop the working daemon
to reopen this gate. Changed units, accounts, foreign owners or another operation's
gate require inspection. Installed service and package acceptance remain pending.
Preserve the journal and check:

```sh
journalctl -u lianli-control-recovery.service -b --no-pager
```

GUI Start, Stop and Restart run in the short-lived host user service
`lianli-control-operation.service`. Once the worker reports progress, closing the GUI
leaves it running. Reopening Settings shows its current progress or final result.
Only one such worker can run per user; the host operation lock also serializes
service actions and daemon settings writes across accounts.

The GUI needs the matching `lianli-control` beside its executable. Source builds must
build both binaries; packages install both in `/usr/bin`. The host needs `systemd-run`
and a reachable user manager. In Distrobox, the host launches `distrobox-enter` and
the helper executes inside the same box as the GUI. No host Lian Li Linux binary is needed.
System-service authorization still uses the desktop Polkit agent.

The worker keeps one bounded private progress record under
`/run/user/UID/lianli-control`, with an owned lock that distinguishes a live worker
from an interrupted verification. Status checks read this local record; they do not
spawn service queries on the telemetry polling path. A failed submission or lost
worker never automatically replays Start, Stop or Restart. Recheck actual services
when the result is unconfirmed. Worker diagnostics are in the host user journal:

```sh
journalctl --user -u lianli-control-operation.service -b --no-pager
```

The transient service is collected after exit and has no automatic restart. Its
lifetime follows the user manager: GUI closure is supported, while logout, reboot
or stopping that manager can interrupt verification. Start/Stop/Restart progress
is session-local. Mode-switch recovery uses the separate durable journal described above.
The worker itself never acquires a device or force-terminates the hardware daemon.
Detached service ownership and literal dollar escaping follow
[systemd-run's execution contract](https://github.com/systemd/systemd/blob/v255/man/systemd-run.xml).

## Host mode selection

The host may select one hardware mode and account in
`/etc/lianli/service-selection.json`. Current daemons check this root-owned record
before acquiring hardware ownership and again after acquiring it. A different mode
or UID exits successfully before configuration recovery or device initialization,
so an unselected user service does not keep restarting at every login. Manual
daemon launches follow the same selection. A missing record preserves existing
launch behavior; invalid, writable, symlinked or unreadable records block startup
with an error instead of being treated as missing.

A service switch can pause startup for both modes using a version 2 selection
record tied to its operation ID. The pause survives reboot and applies to manual,
native service and Distrobox launches. It does not interrupt an already-running
daemon. The coordinator records its stop/recovery phase before pausing startup,
then reopens only the selected mode after publication or restoration while it
still holds hardware ownership. Repeating that step accepts only the same
operation or its already-published selection. Older daemons reject the version 2
record; install preflight requires the `service_startup_gate` capability.

Installation Health reports an unfinished switch while startup is paused.
`select-mode` cannot override that pause. Preserve the selection record and switch
journal for recovery. The native GUI supports explicit recovery and requests an
automatic attempt at launch. Recovery without the caller's manager/home remains
under development.

An administrator can set the selection after stopping every running daemon cleanly:

```sh
sudo lianli-control select-mode --scope user --user-uid "$(id -u)"
```

For system mode, use `sudo lianli-control select-mode --scope system`; this resolves
the packaged `lianli` account. The command takes both the service-operation lock and
the shared hardware lock before atomically publishing the record. It refuses a running
hardware owner and does not stop services, copy settings or change unit enablement.
Run it on the native host. The GUI switch coordinator uses the same protected
publication path. Use the GUI for a managed Distrobox system service so selection
uses the box owner's account.

Distrobox reads the host record through `/run/host/etc/lianli` and verifies its
directory/file identities, root ownership and permissions with host `stat`. This
requires the already-installed `host-spawn` bridge even when no selection exists,
so a hidden host record cannot silently become an unselected container. Run
`distrobox-host-exec true` interactively inside the box to initialize that bridge;
the daemon never downloads it. This adds two bounded host metadata queries when a
record is present, per launch check; ordinary polling does not repeat them. Missing
or mismatched host visibility is actionable startup failure. The host needs no
Lian Li Linux binary just to enforce an already-existing selection inside Distrobox.

Settings and Installation Health show the authoritative selection independently of
unit enablement. Start/Restart reject a conflicting or unverified selection, while
Stop remains available for a verified existing owner. Install current daemon binaries
for every launch route before using this guard; older binaries do not enforce it.
Do not edit or remove the record while hardware ownership is held.

The daemon reports the device/inode identity of its held lock descriptor. Host
diagnostics inspect `/proc/locks` and compare that identity with the actual host
lock file. Distrobox additionally checks the file through its host bridge and
reads the host's kernel lock table, since container PID namespaces can hide host
owners. File PID text is not used as proof. A changed file, symlink, unsupported
lock record or unavailable host view makes ownership unverified. Kernel ownership
is a snapshot and must be revalidated before a service transition.
See the Linux documentation for [kernel lock records](https://man7.org/linux/man-pages/man5/proc_locks.5.html)
and [process start time](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html).

If the GUI cannot load, run `lianli-control diagnose`. This standalone command
reports service state as JSON and has no WebKit, display-capture or USB dependency.
Each systemctl query is bounded to seven seconds and 64 KiB per output stream.
Unavailable queries are reported separately from missing, disabled or failed units.
The normal GUI telemetry poll does not rerun these service queries.

## Service controls

Install the current units and tmpfiles rule, then reload the relevant systemd
manager. The host needs both `/run/lianli-daemon.lock` and
`/run/lianli-control.lock`; the latter serializes service actions across GUI
instances and accounts. Neither lock is created automatically by the GUI.

Save pending configuration changes and close the template editor before using
the controls. Settings writes from every GUI and IPC client take a shared permit
on the host control lock. The permit covers persistence and queued application in
the daemon's main loop, even if the client closes. A service operation waits up to
five seconds for pending writes, then reserves the lock exclusively. New writes
are rejected with a retry message while it runs; reads and guarded graceful-stop
requests remain available. No settings change is automatically retried.
At most 64 settings requests may remain pending; further writes get a retry error
until earlier work completes, bounding the retained lock descriptors.
The operation verifies that the daemon's write lock matches the host lock, including
Distrobox paths. A missing control lock blocks new settings writes with setup
guidance while the daemon keeps running its existing configuration. Installing the
missing tmpfiles rule repairs that case; replacing an already-used lock requires a
clean daemon restart and must never be done during an operation.
System actions use systemd's authorization through your desktop Polkit agent.
An authorization cancellation leaves the requested action unapplied; a timed-out
request is reported as uncertain and is never retried automatically.

Start and Restart require the other mode to be inactive and disabled and global
user startup to be disabled. Stop may resolve an existing conflict, but only for
a verified owner of the selected unit. Manual launches and another user's daemon
are never terminated by guessing a PID. Distrobox controls use the installed
host-spawn route for either managed service mode after host support setup.
The current [Distrobox service recipe](distrobox.md#start-from-the-host-user-service)
passes systemd's invocation ID into the daemon and includes a guarded IPC stop command.
This identifies and stops the container daemon even outside the wrapper's cgroup.
Both binaries remain installed inside the box. The GUI checks structured start/stop
arguments through host `busctl`, authenticated daemon IPC, host lock ownership and,
inside Distrobox, matching PID namespaces and process start times. A stale invocation,
unrelated process or missing host visibility cannot authorize stopping a daemon.

The native units use `KillMode=mixed`, `SendSIGKILL=no` and a thirty-second stop
timeout. The daemon receives SIGTERM and controls its own worker shutdown. A stuck
shutdown is reported without force-killing an unfinished hardware transaction.
The UI requires SIGTERM for stop/restart, normal timeout handling and either no
stop hooks or the verified Distrobox stop recipe. Outdated or overridden units that cannot establish this policy are
rejected; use the supplied unit and reload the manager before rechecking.

The Distrobox recipes use `KillMode=control-group` so systemd sends SIGTERM to
remaining Podman helpers after the guarded stop command completes. Forced kills
remain disabled. The hardware daemon recipe allows 120 seconds for its stop command, which waits up to
ninety seconds after acknowledgement for the daemon process to exit. It monitors a
pidfd rather than polling or signalling a numeric PID. This requires the host's
`pidfd_open` support (Linux 5.3 or newer) and a container policy permitting it; failure
is reported before sending the stop request. See the Linux [pidfd_open documentation](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)
and systemd's [invocation identity contract](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#%24INVOCATION_ID).

Current daemons also advertise graceful shutdown support. Older binaries with the
same development version are rejected by Stop/Restart if they still have the
twenty-second forced-exit watchdog. Update the binary and restart it cleanly.
Signal handling starts before device initialization; shutdown waits for outstanding
device-open workers while retaining the hardware lock. Slow operations produce
bounded warnings instead of terminating an unfinished USB transaction. A timed-out
open cannot launch a duplicate worker while the original is still running.

Successful systemctl submission alone does not complete the UI operation. Stop
requires an inactive unit and a released hardware lock. Start requires a verified
service owner, compatible daemon identity and readable loaded configuration;
Restart additionally requires a new daemon instance. Verification waits up to
ninety seconds plus the current bounded inspection. Individual diagnostic commands
have seven-second deadlines; authorization may take up to two minutes. Timeout or
startup failure leaves the observed service state for inspection and does not
issue another start, stop or kill. The independent worker continues verification
after GUI closure; reopen Settings to read its result within the same login session.

## Start a fresh native installation

Fresh packages enable neither mode. For a desktop login session, start the user service:

```sh
systemctl --user enable --now lianli-daemon.service
```

For control before login, use the system service instead:

```sh
sudo systemctl enable --now lianli-daemon-system.service
```

Choose only one. If a daemon or service is already running, use the switching guidance
below. The GUI's Installation Health page stays available while offline and links here.

Source installations must first install the binaries and supplied user/system units, plus
`packaging/sysusers.d/lianli.conf` and `packaging/tmpfiles.d/lianli.conf` under the matching
`/usr/lib` directories. Run `sudo systemd-sysusers lianli.conf`,
`sudo systemd-tmpfiles --create lianli.conf`, `systemctl --user daemon-reload` and
`sudo systemctl daemon-reload` before selecting a mode. The complete source dependency
and installation commands are in [source installation](building-from-source.md).

Stop running daemons before upgrading from older releases, including manually launched
processes and daemons inside containers. Older container installations may have used a
private lock that did not coordinate with the host.

## Repair a missing lock

Native packages install `lianli.conf` under `/usr/lib/tmpfiles.d`. On the host, run:

```sh
sudo systemd-tmpfiles --create lianli.conf
```

For a source installation, first install the supplied rule from the repository:

```sh
sudo install -Dm644 packaging/tmpfiles.d/lianli.conf /usr/lib/tmpfiles.d/lianli.conf
sudo systemd-tmpfiles --create lianli.conf
```

The rule creates a root-owned regular file that both daemon identities can open. Do not
delete or replace the lock while a daemon is running: existing file descriptors would
continue locking the old file. The presence of the file does not mean a daemon is running;
the kernel releases the lock when the process exits. Symlinked lock files are rejected.

## Distrobox

The daemon opens the host lock at `/run/host/run/lianli-daemon.lock`. It does not use the
container's private `/run/lianli-daemon.lock`. If only the box has the package installed,
copy its tmpfiles rule to the host. Run these commands **on the host**, replacing the box name:

```sh
distrobox-enter --name "<boxname>" -- cat /usr/lib/tmpfiles.d/lianli.conf \
  | sudo tee /etc/tmpfiles.d/lianli.conf >/dev/null
sudo systemd-tmpfiles --create lianli.conf
```

USB rules and group membership also belong on the host; follow the
[Distrobox installation guide](distrobox.md).
The host user service must enter the box to launch the daemon. Do not enable a second
native system daemon alongside it.

Managed user and system wrappers can run the daemon inside the same box under its
non-root host owner, using separate configuration directories. When both loaded
wrappers belong to the current box, the GUI offers mode switching and recovery.
System start, stop and restart also verify the protected deployment before using
the host manager. Service actions preserve the selected startup mode.
Requests go through the host control helper, which checks the protected deployment
record before coordinating the switch. The helper can come from the native package
or be installed separately at `/usr/local/libexec/lianli/lianli-control`.
In the box's GUI, stop the current daemon, open Settings and choose **Set up host
support**. Review the configuration and launch paths, then authorize installation.
Setup preserves custom support files for review and does not enable either hardware
service. Afterwards, choose the mode to enable and start. **Repair host support**
reuses the protected deployment's existing paths.

Service diagnostics invoke an already installed `/usr/bin/host-spawn` or
`/usr/local/bin/host-spawn` binary directly, using the visible host user's session
bus. The host also needs `/usr/bin/systemctl` and `/usr/bin/timeout`; timeout bounds
the host query even if its bridge disappears. Missing prerequisites are reported
without installation. Install the bridge deliberately inside the box before
rechecking. The application does not invoke the wrapper's automatic download path.
See [Distrobox host execution](https://distrobox.it/usage/distrobox-host-exec/) and
[host-spawn](https://github.com/1player/host-spawn) for the supported host bridge.

If the host's runtime directory is not visible, startup fails with a container integration
error. Restore Distrobox's host filesystem integration instead of creating a separate lock
inside the box. Generic containers without verified host integration are unsupported.
Distrobox documents its host filesystem sharing and volume options in
[distrobox create](https://distrobox.it/usage/distrobox-create/).

## Switch native service modes

Configurations are separate: user mode normally uses `~/.config/lianli/config.json`, while
system mode uses `/var/lib/lianli/config.json`. Templates, presets and media must also be
accessible to the destination daemon. Back up both configurations before transferring settings.

To switch from user mode to system mode:

```sh
systemctl --user disable --now lianli-daemon.service
sudo systemctl enable --now lianli-daemon-system.service
```

To switch back:

```sh
sudo systemctl disable --now lianli-daemon-system.service
systemctl --user enable --now lianli-daemon.service
```

Check for global user-service enablement if the user service starts again on login:

```sh
systemctl --global is-enabled lianli-daemon.service
```

If globally enabled, the administrator must disable that default before selecting system
mode. Stop any manually launched daemon cleanly as well. A successful start command is
not enough: confirm the intended service is running and the GUI reports its configuration
mode and file.

## Inspect saved state before migration

`lianli-control` can inventory a configuration directory without starting a daemon
or changing services:

```sh
lianli-control inspect-state --config '/path/to/config.json' --working-directory '/daemon/working/directory'
```

Run it under the account and in the container/namespace that owns the source state.
Supply the daemon's actual working directory: relative template paths use it,
while direct LCD paths and sensor fonts use the configuration directory.

The JSON summary includes file, profile, template and asset-reference counts,
structural issues and a generation fingerprint. It inspects `config.json` (or the
specified configuration filename), `lcd_templates.json`, `rgb_presets.json` and
saved `profiles/*.json`, including profiles and templates that are not currently
active. Corrupt files and unsupported profile schemas cause an explicit error.
Stored media paths and sensor fonts are included even when an LCD currently uses
a different media type. Template files shared by several LCDs are inventoried once.
State files and the profiles directory must not be symlinks. Missing auxiliary
files are allowed; a missing source configuration is an error.

Add `--check-assets` to open every saved media and font reference under the command's
actual account and mount namespace. The report identifies the UID and each failing
profile, template or widget; it includes inactive profiles, unused templates and media
saved under a currently different LCD mode. Relative direct media paths use the config
directory; template children use the supplied working directory. Repeated paths are
opened once per check, with each affected reference counted. Asset symlinks are followed,
but non-regular targets are rejected before opening them for reading. Settings changes
during the check invalidate the result. Invalid LCD settings are also reported for saved
profiles, before media copying starts.

The command exits unsuccessfully if state or asset access fails, while retaining its JSON
report on stdout. It caps issue details at 32 and checks cancellation/time limits between
file operations; an unavailable filesystem can still delay an individual system call.
This validates readability, not codecs or the permissions of another account. Run it
under the intended daemon account and namespace; running it as root does not establish
access for the system service's `lianli` account. Service switching must perform that
destination-account preflight automatically before stopping the current daemon.

The inventory retains original JSON bytes, including unknown fields, for migration
preparation. It allows 16 MiB per state file, 64 MiB total, 256 profiles and 4,096
asset references. The inventory command does not copy media or switch
services. A migration must check source generations before and after clean
shutdown, verify destination access and retain rollback state before replacement.

## Preflight a native destination

The standalone control helper can authorize and inspect a native destination while
the current hardware daemon continues running:

```sh
pkexec /usr/bin/lianli-control check-destination --scope system
pkexec /usr/bin/lianli-control check-destination --scope user
```

User mode means the caller identified by Polkit; system mode uses the named `lianli`
user and group. The child helper drops to that UID, primary group and complete
supplementary group list before accessing settings or assets. It clears inherited
environment variables and prevents privilege gain through subsequent execution.
The operation serializes with settings writes and other service actions. It does not
stop/start a daemon, enable units, select a mode or publish configuration/media.

Preflight creates missing destination directories, then checks actual write access
using a temporary file that is removed. System setup creates only `/var/lib/lianli`,
with its intended owner/group; it refuses existing symlinks or wrong ownership and
never recursively changes ownership. User setup runs under the caller's account.
Existing config, presets, templates and profiles remain unchanged. A missing config
is accepted only for a fresh directory without saved auxiliary state. Interrupted
state transactions must be recovered before another switch is prepared.

Missing media is reported in the destination inventory. When copying current
settings, missing files referenced only by the old destination settings do not
block their backup and replacement. Media being copied or selected for playback
still requires successful access and decoding.

The check requires the installed supplied native unit and a reloaded manager.
Simple, one-assignment-per-line `Environment=` overrides are supported for
`RUST_LOG`, `DISPLAY`, `WAYLAND_DISPLAY`, `HYPRLAND_INSTANCE_SIGNATURE` and the
obsolete `LIANLI_ENABLE_HW_VIDEO` variable (which has no effect). Execution,
account, namespace and configuration-path overrides remain unsupported and report
the conflicting directive. Systemd's default user working directory is accepted.
It verifies the installed daemon's version/protocol and switching capabilities
through `lianli-daemon capabilities`, which returns before configuration, ownership or
hardware initialization. Missing runtime libraries or outdated binaries fail here.
Custom launch wrappers, namespace overrides and Distrobox-to-system migration need
their separate supported routes; preflight does not assume they behave like native units.

User config paths come from structured user-manager environment values, preserving
spaces and literal characters. Relative paths follow the service's working directory:
the user home for the supplied user unit and `/` for the system unit, as documented by
[systemd's working-directory contract](https://github.com/systemd/systemd/blob/main/man/systemd.exec.xml).
The authorized parent verifies the user manager's namespace and live groups
before and after destination-account inspection. File-access checks still run as
the destination account; they do not require access to the manager's protected
`/proc` namespace links. Stale groups
require a complete logout/login, or a reboot when lingering keeps the manager alive.

The report contains account/context identity, state generation and asset counts.
All saved dependencies are checked, including inactive profiles and template children.
This is a readability/setup check; codec and security-policy behavior still need final
destination/startup verification. The child has a thirty-second deadline with bounded
diagnostic output, though uninterruptible filesystem calls can delay process reaping.
The native coordinator connects these checks to copying and rollback. The GUI
selector is available; recovery with an unavailable manager/home is still being integrated.

## Native switch coordinator

The installed native control helper connects preflight, optional copying, clean
shutdown, publication, exclusive startup selection and verified daemon startup:

```sh
pkexec /usr/bin/lianli-control switch-mode --scope system --carry-settings
pkexec /usr/bin/lianli-control switch-mode --scope user
pkexec /usr/bin/lianli-control recover-switch
```

Omitting `--carry-settings` uses the destination's saved settings, with decoding
under that account before shutdown. An empty destination uses daemon defaults.
Both choices recheck settings and saved asset identities before and after stopping
the source. Working directories, configuration paths and service accounts are
checked again before startup so a changed user-manager environment cannot silently
select another configuration. The operation lock spans the transaction; hardware
ownership remains reserved during publication and startup selection.

Failures attempt rollback and retain the journal if recovery cannot be verified.
Recovery must be authorized by the original GUI user. A daemon that is still
stopping, owns hardware unexpectedly or cannot answer identity checks is never
force-killed. Let its clean shutdown finish and recover again; preserve journals,
backups and prepared imports. A reopened coordinator resumes restoration stages
without repeating completed publication. Account changes or older journals lacking
working-directory information require administrator inspection.

After verifying the selected or restored daemon, the coordinator removes the
generated preparation files under their destination account. Current settings,
imported media, backups and source files remain available. Cleanup records its
progress before renaming the preparation directory, so recovery can finish an
interrupted removal without switching modes again. Unknown files, replaced
directories or pending state publication stop cleanup and preserve its receipt
and switch journal for inspection. Do not manually remove marked preparations.

This is implementation-stage functionality awaiting installed-account and service
integration validation. Full recovery across boot/logout is still open despite
the automatic triggers above. Do not treat this stage as release acceptance.

## Prepare state for another native service account

The standalone helper can prepare the opposite native mode's saved settings and
media before either service is stopped. This requires `pkexec`, desktop authorization,
matching binaries and the native destination preflight described above:

```sh
pkexec /usr/bin/lianli-control prepare-transfer --scope system
pkexec /usr/bin/lianli-control prepare-transfer --scope user
```

For `--scope system`, the source is the authorized desktop user's supplied user
service configuration; for `--scope user`, the source is the named `lianli` system
account's configuration. Paths follow those installed units' environment and working
directories. This command does not import a manually launched daemon's alternate
configuration or working directory. Save source settings before preparation.

The authorized parent holds the host operation lock while two helpers run under
their respective accounts and groups. The source opens each asset and passes its
read-only descriptor over a private inherited socket using
[Unix descriptor passing](https://man7.org/linux/man-pages/man7/unix.7.html). The destination copies from
that descriptor, so private source directories do not need broader permissions.
The privileged parent never opens source media. Raw settings cross through
[sealed memory files](https://man7.org/linux/man-pages/man2/memfd_create.2.html) with checksums. Profiles, presets, unused templates and child assets
are included; unknown JSON fields survive path rewriting.

Preparation uses one file descriptor at a time, at most 2 GiB per asset, 8 GiB of
staged media, 4,096 assets and 64 MiB of settings with a 16 MiB per-file limit. The
send/receive helpers each have a 2 GiB address-space ceiling and a ten-minute
deadline. Stricter inherited memory limits are preserved. The address-space limit
includes allocator overhead and mapped files, not just parsed JSON. A helper
that exceeds it can exit before producing an error message, leaving a transfer
failure for the coordinator to report and clean up. Uninterruptible filesystem
calls can delay reaping. Source file identities and both configuration generations are rechecked
before final acceptance. Ordinary failures remove the prepared files and preserve
the source and destination settings.

Success prints an operation ID and a private `.lianli-migration-ID-*` directory in
the destination configuration folder. It contains staged settings, deduplicated
media and a manifest. Settings refer to the future `media/imports/ID` directory;
those references become usable only after the switch transaction publishes it.
Current settings and startup selections remain unchanged. Preparation validates
settings structure and the copied assets under the destination account. Each unique
path/type is checked once, including inactive profiles and unused template children.
Images and fonts use the application's parsers; videos require a decoded first frame
from software FFmpeg. GIFs pass both parsers used by the JPEG and H.264 playback paths.
Dormant paths whose media type is unknown receive a readability check. File changes
during validation abort preparation. This is an initial decoding check, not proof
that every later frame in a long clip is valid. The native coordinator handles
publication/rollback; the GUI choice and final integration validation remain open.

Each decoder receives a read-only file descriptor and runs separately from hardware
control with a 25-second wall deadline, 20-second CPU limit and 1 GiB address-space
limit. Image decoding allows at most 8192 pixels per dimension and 64 MiB of decoder
allocations; custom fonts are limited to 32 MiB. FFmpeg uses one decoder thread,
software decoding and a 16-megapixel limit. It replaces the helper process so the
same supervisor still owns its deadline and cleanup. No frames or transcodes are
written to disk. Empty video output is rejected using FFmpeg's
[empty-output check](https://ffmpeg.org/ffmpeg.html#Advanced-options).
Older prepared transfers without completed decoder validation must be prepared again.

Only one pending preparation is allowed in a destination folder. An interrupted
helper can leave a pending directory; inspect its manifest and use its operation ID
to discard it under the destination account. For example:

```sh
lianli-control discard-transfer --config "$HOME/.config/lianli/config.json" --operation-id ID
sudo -u lianli /usr/bin/lianli-control discard-transfer --config /var/lib/lianli/config.json --operation-id ID
```

Use the actual user configuration path if `XDG_CONFIG_HOME` differs. Discard refuses
root execution, replaced directories, foreign accounts, mismatched operation IDs
and unrecognized manifests. It removes only unpublished preparation, without
changing saved configuration or published imports. A corrupt or newer manifest
requires inspection rather than an automatic recursive cleanup.

## Interrupted state publication

During migration, a hardware reservation must retain the existing shared flock
after the old daemon exits and throughout state publication. Reservation checks
the previously inspected file identity, rechecks the host object for Distrobox,
and refuses a missing, symlinked, replaced or already-held lock. It never kills an
owner, creates a fallback file or changes the PID text. The reservation is released
before starting the destination daemon, whose identity must then be verified.

The account publication helper inherits the coordinator's operation and hardware
reservation descriptors over a private channel. It verifies both against the fixed
host lock files and keeps them until publication, restoration or recovery ends.
Closing the helper's copies does not explicitly unlock the coordinator's copies;
Linux associates these locks with the shared
[open file description](https://man7.org/linux/man-pages/man2/flock.2.html).

Before replacing settings, the helper sends its prepared backup name and waits.
The coordinator must durably record that backup before acknowledging publication.
A failed acknowledgement leaves the current settings intact and a recoverable
state journal. The supervisor waits for helper exit, including failed or timed-out
work, before it can release reservations. The journal component records this
acknowledgement before permitting account publication or restoration. The native
startup/rollback coordinator is connected; its GUI selector remains open.
The hidden `publish-state` command requires inherited
descriptors and is not a standalone service-switch command.

The authorized native service-action backend retains the coordinator's operation
lock while inspecting services through a helper running as the GUI caller.
User-service requests run as that caller with the caller's session bus and a
fixed service name; system-service requests run under the authorized coordinator.
It never addresses root's user manager. Daemon observations must match the
previously inspected PID, process start time, account, cgroup and hardware lock.
Account/group changes invalidate preflight. Helpers have bounded output and
deadlines, and a timed-out action is not replayed. This backend reuses the existing
graceful stop/start verification inside the native switch sequence.

Startup selection is coordinated separately from starting or stopping a daemon.
While the hardware reservation and startup pause are held, the journal's
destination-selection step disables the old mode in both persistent and runtime
configuration, verifies disablement, enables the chosen mode persistently, then
verifies the result before reopening startup. Commands use fixed unit names and
never include `--now`. These operations follow systemd's
[enablement and runtime-option semantics](https://github.com/systemd/systemd/blob/v255/man/systemctl.xml).

Rollback restores the recorded disabled, persistent or runtime-only startup
choice. The journal records its originating boot; runtime-only enablement is not
recreated after reboot. Older journals without a boot identity require manual
recovery if they contain runtime-only startup. Unknown/masked/overridden units,
unverified global enablement and conflicting requested startup modes block this
operation. Global user enablement must be resolved beforehand because switching
does not change other users' startup configuration. The native coordinator invokes
these helpers; full recovery with unavailable user-session resources remains open.

The native switch journal uses `/var/lib/lianli-control/switch.json` in a
root-owned private directory. It retains the original service accounts, startup
selection, prepared transfer, backup identities and last phase. Updates are
atomic and synchronized to storage; an incomplete or corrupt record prevents
another journal from replacing it. Completion retains only `last-switch.json`.
Preserve these files alongside the account's state backups when reporting a
recovery failure. The daemon startup gate is connected to the journal's stop,
selection and restoration phases. Explicit and automatically requested native
recovery are connected to service orchestration; full boot/logout handling still
needs completion.

Restoration retries reuse the recorded undo backup, checking that it restores
the original destination state. Interrupted writes can be completed without
creating another backup, including when completion was not acknowledged. Changed
live files, mismatched undo records and damaged saved content stop restoration
and preserve the evidence for recovery.

State publication keeps private `.lianli-state-backup-*` directories containing
the previous and proposed config, presets, templates and profiles. The transaction
journal is `.lianli-state-transaction.json`. Known files are replaced atomically
one at a time; the journal covers the complete operation. Unrelated files in the
configuration directory are retained.

The transfer publication library verifies the saved preparation receipt, settings
content and media identities under the destination account. It records recovery
state before moving the prepared media directory into `media/imports/ID`, without
overwriting an existing import. Settings are published only after that move. Media
checks inspect bounded metadata; they do not copy or decode large assets while
hardware ownership is reserved. The native service switch calls this library
after final preflight and clean shutdown.

On startup, after acquiring hardware ownership and before loading configuration
or opening devices, the daemon rolls back an interrupted publication. A completed
commit keeps its new state. Recovery refuses to overwrite externally edited state
or use corrupt backups; preserve the journal and backup directories when reporting
such a failure. A live publication blocks concurrent recovery.

If a committed journal remains, recovery also verifies its published media before
opening devices. Rollback retains imports because saved backups can reference them.
Preparations marked as entering publication cannot be removed with
`discard-transfer`; their recovery and cleanup belong to the switch transaction.

Backups retain original JSON bytes and Unix modes/groups. Each saved state is
limited to 64 MiB, and preparation stops at 16 migration backup directories.
Service switching and backup controls are available in Settings. The read-only
`inspect-state` command does not publish or restore state.
