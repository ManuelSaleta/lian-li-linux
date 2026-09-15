# Service modes and daemon ownership

Run one hardware daemon at a time. The user service runs in your login session; the system
service runs as the `lianli` account. Both use the same host lock at
`/run/lianli-daemon.lock`. A missing or inaccessible lock now prevents startup instead of
allowing separate hardware owners.

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
[Distrobox installation guide in the README](../README.md#distrobox-containers).
The host user service must enter the box to launch the daemon. Do not enable a second
native system daemon alongside it.

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
