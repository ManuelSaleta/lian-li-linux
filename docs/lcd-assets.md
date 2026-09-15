# Managed LCD media

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
