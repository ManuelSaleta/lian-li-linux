# LCD media and template file access

Font preparation checks the selected font at the scaled text size. If a glyph
exceeds the raster limit, preparation reports the affected sensor field or template
widget. Reduce the font size or choose another font, then Save to retry.

Text drawing also has a shared per-frame work limit. Exceeding it rejects the
frame before encoding or submission and reports a rendering error. Reduce text,
font sizes or overlapping text widgets, then Save to retry. Unchanged cached
widgets do not consume raster work again.

Settings → Managed media storage lists retained imports for the connected daemon.
Inspection counts hard-linked aliases once per import and checks both saved state
(including backups) and assets retained by the running daemon. New imports carry
ownership receipts; older unmarked imports and manually changed files remain
unverified. **Review files** reads the selected import's contents, shows their
hashes and refreshes its saved/runtime reference checks. Hard-linked aliases share
one content read and file lists are paginated. Storage inspection and review do
not delete files.

After reviewing an unreferenced import, explicitly confirm **Remove reviewed
directory** to delete its copies. The daemon checks contents and references again
while excluding configuration writes, media preparation and import publication.
Files observed during the current daemon session remain protected until restart.
Original source files are preserved.

An interrupted removal remains listed. Inspect storage, review its remaining and
missing files, and confirm again to resume. Recovery refuses replacements and
changed content. Ownership records remain until removal is durable.

Receipts whose published directory is missing also appear in the inventory.
Reviewing these shows no retained media files; confirmed removal retires only
the receipt after checking references and that no publication has appeared.
Invalid receipts and unmarked legacy storage remain protected.

The daemon loads your LCD content. In user mode it runs as your login user; in system mode
it runs as `lianli`. A file selected in the GUI can therefore be inaccessible to the system
daemon. Inside Distrobox, the path must also exist in the daemon's container.

The **LCD** page checks selected media and custom-template dependencies automatically
after edits. The template editor also checks unsaved backgrounds, widgets and fonts.
Warnings identify the affected LCD, template/widget and file. Use **Recheck
media access** after repairing permissions or restoring a mount. The check runs through
the selected daemon, using its account and container view, and reports its numeric UID.
It does not decode content or change the running display. Older daemons need an update
and clean restart before this check becomes available.

Checks accept at most 1 MiB of configuration, inspect at most 1,024 file dependencies,
and show the first 32 failures with the total failure count. Only one check runs at a
time. Unavailable storage produces a timeout or busy message; it must not stall cooling
control. A successful access check establishes readability at that moment, not that
the file is valid media.

Preflight limits identifiers to 256 bytes, file paths to 4,096 bytes and templates
to 1,024 widgets. Oversized input produces an explicit error instead of expanding
an unbounded dependency report.

Configuration/profile and IPC parsing enforce at most 256 LCD entries;
template files and template-edit requests allow at most 1,024 templates, each
with at most 1,024 widgets. These collection limits apply during parsing, before
the entire list can allocate memory. Existing state files also have a 16 MiB byte
limit. Oversized collections are rejected rather than silently truncated.

Still images, sensor backgrounds and custom-template images use the same decoder
limits during preparation and managed-import preflight: at most 8,192 pixels per
dimension and a 64 MiB decoder allocation budget. Resize oversized source images
before selecting them. Codec internals and later resizing/color conversion mean
this budget is not a limit on total daemon memory. Invalid sensor backgrounds
fail preparation instead of silently disappearing from the gauge.

Template render buffers also have an 8,192-pixel dimension limit and a 64 MiB
RGBA limit per buffer. This includes resized backgrounds, image/video widgets
and the larger intermediate buffers used for smooth edges. Analog-clock number
canvases use the same limits. Reduce oversized widget dimensions or clock number
sizes if preparation reports a render-dimension error.

Sparkline history uses between 2 and 4,096 samples in both preview and playback
(the default remains 60). Values outside that range use the nearest limit.
History buffers are allocated during preparation and included in the shared
retained-buffer budget; live sampling replaces the oldest entry when full.

Prepared media share a 1 GiB retained-buffer budget across displays, including
replacements being prepared. It covers static frames, encoded video frame
collections, sensor backgrounds/fonts, template canvases, widget image caches,
decoded widget video frames and font bytes. A failed reservation leaves existing assets intact;
reduce video lengths or widget sizes before retrying. Reservations are released
when the last user of the asset or frame collection stops. This is not a total
daemon-memory limit: metadata, H.264 temporary files and transient decoding/rendering
allocations are outside this buffer budget.

Sensor and template fonts must be regular files no larger than 32 MiB. The same
font parser and size limit apply during normal preparation and managed-import
preflight. Invalid or oversized fonts fail preparation with the affected path.
Individual glyph rasters are limited to 8,192 pixels per dimension and 64 MiB.
Glyphs exceeding these limits are omitted; reduce the text size if they disappear.
Sensor labels/units and template text/format fields are limited to 4,096 UTF-8
bytes each. Scaled text sizes and letter spacing must be finite and no greater
than 8,192 pixels in magnitude for spacing. Numeric value and sparkline formats
support up to 64 decimal places. Exceeding these preparation limits reports the
affected field or widget in Installation Health; reduce it and Save again.
Digital-clock formats are checked during preparation, including a 4,096-byte
expanded-output limit. Runtime expansion remains bounded as the date changes;
an invalid or over-limit result falls back to `HH:MM`.

Video/GIF frames prepared for JPEG or PNG playback and animation frames cached by
custom video widgets are limited to 8,192 retained frames and 256 MiB per animation. Exceeding
either limit fails preparation instead of truncating the animation. Use a shorter
clip or a smaller video widget. Source decoding also uses the image limits above;
simultaneous displays and widgets consume additional memory.

Video preparation reads decoded frames from FFmpeg through a bounded pipe instead
of writing temporary frame directories. Playback frames use the target screen's
image format, JPEG quality, orientation and payload limit. Prepared H.264 files
must remain below 256 MiB, or a stricter file-size limit inherited by the daemon.
Reaching that limit fails preparation and removes the partial temporary file;
use a shorter clip. This is a per-file limit, not a shared storage quota across
all displays.

H.264 preparation also shares a 2 GiB temporary-storage reservation pool within
the process. Ordinary transcodes reserve 256 MiB before starting; completed files
retain only their actual byte count. Pixel cleaner reserves its input frames and
output separately. Capacity becomes available after the final owner releases the
temporary directory and cleanup succeeds. A cleanup failure is logged with its
path and keeps that capacity charged. Files left by an abrupt process crash are
outside the new process's accounting.

When you save LCD media, preparation checks file access under the daemon's account. This
covers direct images/videos/GIFs, sensor backgrounds and explicit fonts, template backgrounds,
image/video widgets, and explicit fonts on labels, values, clocks and sparkline axes.
Symlinks are followed; the target and every ancestor directory must be accessible. Files
must be regular files, not directories or device nodes.
Image, GIF and APNG loading rechecks the opened file before decoding. Replacing a saved image
with a pipe is rejected without waiting for a writer or blocking later preparation.

**Installation Health** shows preparation failures with the affected template/widget
and path. A missing, unreadable or undecodable child fails the replacement instead of
silently leaving part of the template blank. Existing working content stays active where
available. After repairing files or permissions, choose **Retry failed media** in
Installation Health. It uses saved media settings without saving unrelated edits.
Active preparation and pixel cleaning finish before the retry, and healthy cached
assets are reused. If you edit the selected path or template, **Save** those changes.
JPEG sensor/template render failures stop that source and report failure in Health.
They do not retry or log repeatedly until you request recovery.
Replacing or stopping media wakes scheduled renderer waits, including long GIF
frame delays and H.264 restart backoff. A frame already rendering or an encoder
write already in progress may finish afterward.

## System daemon

Use a location readable by both your GUI user and the `lianli` account. For non-private
media, a shared read-only location can be prepared on the host:

```sh
sudo install -d -m755 /usr/local/share/lianli/media
sudo install -m644 '/path/to/your/video.mp4' /usr/local/share/lianli/media/video.mp4
```

Select the copied file in the GUI. Update background, widget and font paths in custom
templates too. These permissions make the copied content readable to other local users.
For private content, use access controls appropriate to your system or the user daemon.

## Relative paths and removable files

Direct LCD paths are resolved against the daemon's configuration directory when config
is loaded. Catalog templates install with resolved asset paths. Hand-written relative
template paths currently follow the daemon's working directory, so absolute paths are
more predictable across service modes. Migrating a template must include its child files.

Access can change after a check: mounts can disappear, permissions can change, and files
can be replaced. Preparation verifies access again on use. A playing asset may already
be cached in memory or transcoded temporary storage; continued playback does not establish
that its original file remains available for the next preparation.
## Managed import storage

On the **LCD** page, use **Copy into managed storage** to copy the displayed LCD
selections and their custom-template assets. Native imports require the matching
installed helper, user service manager and desktop authentication agent.
Distrobox imports support either daemon mode running in the same box and account.
The host user service manager launches the worker through `/usr/bin/distrobox-enter`,
using the installed `/usr/bin/lianli-control` inside the box. The host command
bridge described in [the Distrobox guide](distrobox.md) is required.
When using the host GUI with a managed Distrobox deployment, the host helper
reads selected sources under your host account and transfers them to the verified
container destination. The selected configuration must match the installed mode.
Results include the state directory resolved inside the destination, so guest-only
state paths do not need to exist on the GUI's host filesystem.
The worker continues if you close the GUI; **Check import progress**
retrieves its outcome while the desktop session remains available.

After copying succeeds, review the result and confirm **Stage copied paths**.
This replaces the LCD drafts and matching template drafts; it does not apply
them immediately. Use **Save** to apply, or **Reload** to discard those drafts.
Saving preserves unrelated templates and refuses matching templates changed by
another writer. If template saving succeeds but LCD saving is unconfirmed, the
error explains that partial outcome; retry Save or reload to inspect settings.
Changing the connected daemon requires reviewing the result again. Originals
are retained, and discarding a draft does not remove the managed copies.

Distrobox imports verify the daemon account, filesystem namespace, configuration
path, instance and shared host write lock before publishing. Restarting the daemon
during copying requires reviewing and retrying the import. For native imports, a manually launched daemon with
a custom configuration path must match the selected service destination before
the helper will copy.

Publishing imported media requires the committed `media/imports` store plus the
incoming import to fit within 8 GiB. Hard-linked aliases count once; retained
generations count toward the limit. Inspection is bounded to 1,024 imports
including the new one, 65,536 file entries and five-second checks between file
operations. Links, nested directories, special files or unreadable imports
prevent publication. Source files and existing imports are preserved on failure.
This limit is separate from the catalog template asset quota.
