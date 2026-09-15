# Managed LCD media

Desktop mode uses `lianli-session` in the graphical login session to capture frames
for the daemon. Packages install its user unit and login trigger so capture does
not depend on opening the GUI. Source installations must install the helper beside
the daemon and GUI, plus `packaging/systemd/lianli-session.service` and
`packaging/desktop/com.sgtaziz.lianlilinux.session.desktop` in the user-unit and XDG
autostart directories. EVDI capture requires its userspace library and kernel module.
The capture coordinator verifies the active graphical session and bounds worker
startup, communication and teardown. Runtime video/FPS changes recreate its encoder.

Playback prepares replacements separately and retires old sources without blocking
the streaming loop. Settings reports the active transfer method, encoder and FPS
limit. Retry failed media uses saved settings without saving unrelated drafts.

The LCD page can copy selected media and custom-template assets into managed storage
for the selected native daemon. Review the result, stage its copied paths and Save.
The copy runs independently of the GUI and retains its outcome for inspection.
Copying does not grant the system daemon access to private source directories.

Settings lists catalog and managed-media storage for the connected daemon. Review
files before removal. Saved configuration, profiles, backups and assets observed by
the running daemon protect referenced files. Legacy directories without verified
ownership records remain protected from automatic removal.

Removal rechecks file contents, identities and references while excluding settings
writes, preparation and import publication. Interrupted removal retains its record
for a new review and explicit confirmation. Never delete receipts to bypass review.

Catalog downloads are bounded and cancellable. Their installation status survives
GUI reconnection, and shutdown cancels preparation before publishing new templates.
