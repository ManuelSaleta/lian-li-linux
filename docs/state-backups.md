# Configuration and template backups

The daemon saves configuration, RGB presets, device profiles and user templates
through the same atomic persistence path. It writes and syncs a unique sibling
temporary file, then replaces the destination. A previous valid JSON file is
retained as `<filename>.bak` before replacement. Only one previous version is kept;
the next successful save replaces that backup.

New state files are private to the daemon account. Existing Unix permission bits
are preserved. State paths must be regular files; symlinks and special files are
rejected. Each JSON state file is limited to 16 MiB. This limit does not apply to
the media files referenced by configuration or templates.

If the existing file contains invalid JSON, saving reports an error and preserves
both that file and any previous backup. Failed configuration and preset writes
do not replace the GUI-visible daemon state or queue a configuration reload.

Settings shows the active daemon's configuration path. Its sibling files include
`lcd_templates.json` and `rgb_presets.json`; device profiles live in `profiles/`.
The system daemon normally uses `/var/lib/lianli`. Distrobox paths must be handled
inside the same filesystem namespace as the daemon.

To recover manually, stop the selected daemon cleanly, keep a separate copy of the
damaged file, and inspect its `.bak` file. Restore the backup to the original
filename using the same daemon account and permissions, then restart the daemon.
If parsing still fails, keep both copies and the error from the service journal.
Do not delete the shared ownership lock or start a second daemon to perform recovery.

Backups can contain personal paths and device configuration. Review them before
sharing them in an issue report.
