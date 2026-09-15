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

Installation Health reports the daemon's last configuration/template load result,
including validation warnings, duplicate entries and parse/read errors. A failed
configuration reload retains the last successfully loaded configuration. A failed
template read retains templates already in memory; catalog installation reports
the error instead of treating an unreadable collection as empty. Template files
over 16 MiB are rejected on read as well as write.

These findings remain available if the external runtime diagnostic helper is missing.
Recheck displays the last load attempt; it does not reload files or restart devices.
After repairing state, save through the GUI or cleanly restart the selected daemon
and Recheck. A deferred configuration application during pixel-cleaner shutdown is
shown separately from a successful load.

Settings shows the active daemon's configuration path. Its sibling files include
`lcd_templates.json` and `rgb_presets.json`; device profiles live in `profiles/`.
The system daemon normally uses `/var/lib/lianli`. Distrobox paths must be handled
inside the same filesystem namespace as the daemon.

In **Settings → State backups**, choose **Find backups** to list the connected
daemon's previous-version files, then **Preview** to inspect one without changing
settings. The list distinguishes **Previous save** (`.bak`) and **Before restore**
(`.before-restore`) copies. Discovery includes the fixed state files and profile backups; unrelated
files are excluded. Reads reject symlinks and special files and enforce the 16 MiB
limit. The preview shows at most 64 KiB and explicitly labels truncation or invalid
JSON. Preview also checks the file's state schema. Configuration previews use the
startup loader's migrations and validation warnings, resolving relative media paths
against the configuration destination. Profile previews reject unsupported schema
versions and names that disagree with their filenames. These checks do not prove
media can decode or that disconnected devices will accept the settings. Switching
daemon instances clears the old list and preview. Discovery scans at most 512
profile-directory entries and supports up to 256 profile backup files across both kinds.

Choose **Restore backup** after reviewing and confirming its contents and warnings.
The daemon rechecks the backup hash and schema before replacing the selected file.
It preserves the replaced bytes, even malformed JSON, in a sibling
`.before-restore` file, keeps the selected `.bak` intact, and requests a normal
daemon reload. This can change active cooling, lighting and display settings.
The success message confirms disk replacement and a queued reload, not hardware
acceptance. Review active settings and installation/media errors afterward.

An existing `.before-restore` file blocks another replacement so recovery history
cannot be silently overwritten. Use **Find backups → Preview**, review that copy,
and separately confirm **Delete backup** when it is no longer needed. The same
action can remove an ordinary `.bak` file. Deletion verifies the reviewed bytes,
changes no current settings and removes no media. Malformed JSON can be deleted;
invalid UTF-8 is visibly replaced for preview while the hash covers the original
bytes. Symlinks, special files and oversized files remain rejected. Preserved files
are available for inspection or deletion here; manual recovery can use them after
stopping the daemon cleanly.

If an operation times out, it may still finish;
reconnect and inspect current state before retrying. Other settings writes are
rejected while restore is in progress. The daemon does not hold its shared state
lock while reading or replacing backup files.

To recover manually, stop the selected daemon cleanly, keep a separate copy of the
damaged file, and inspect its `.bak` file. Restore the backup to the original
filename using the same daemon account and permissions, then restart the daemon.
If parsing still fails, keep both copies and the error from the service journal.
Do not delete the shared ownership lock or start a second daemon to perform recovery.

Backups can contain personal paths and device configuration. Review them before
sharing them in an issue report.
