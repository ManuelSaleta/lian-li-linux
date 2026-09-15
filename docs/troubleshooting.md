# Installation troubleshooting

## GUI is offline

Check the selected daemon and its logs:

```sh
systemctl --user status lianli-daemon.service --no-pager
journalctl --user -u lianli-daemon.service --no-pager
```

For the system daemon, use `systemctl status lianli-daemon-system.service` and
`journalctl -u lianli-daemon-system.service`. Follow
[service modes and ownership](service-modes.md) if the shared lock is unavailable.

## Changes are disabled

The GUI and daemon must have matching versions and IPC capabilities. Update both,
restart the selected daemon cleanly and refresh the GUI before saving. Requests
from an earlier daemon instance are rejected without changing saved state.

For damaged configuration or template JSON, see [state backups](state-backups.md).
