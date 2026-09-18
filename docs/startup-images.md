# Startup images

Use **Startup Image** on a supported LCD card to choose a PNG, JPEG or BMP,
position and rotate it, then upload it. Zooming out fills the remaining area
with black. Configure live media for the physical USB LCD first. Playback
pauses during upload and resumes afterward. Saved live-media settings stay
unchanged. Storage writes happen only when you request an upload.

Startup images are available on:

- TLV2 Wireless LCD connected by USB, with persistence confirmed on hardware.
- SL Wireless LCD connected by USB.
- Wired TL LCD.
- TL Flex and SL-INF Flex LCDs with their matching receiver.

These models have reachable image-saving paths in the vendor application and
backend. The vendor saves some startup images through its normal image-selection
flow. Linux uses an explicit upload action to avoid repeated storage writes.
Only TLV2 Wireless has hardware-confirmed persistence so far. On the other
retained models, check the image after a full power cycle before relying on it.

Universal Screen 8.8, Lancool 207, Vision 9.2, HydroShift II Circle/Square and
OLED Curve do not expose startup-image upload in Linux. Their dormant vendor
SDK helpers do not establish a working application feature. Universal Screen
hardware testing also showed that uploads to its boot-logo directory did not
take effect. The GUI hides the action and the daemon rejects upload requests
for these models. Normal LCD playback and existing media settings are unaffected.

Only one upload runs at a time. Configuration changes are saved and applied
after the upload. Cancellation stops preparation or a queued upload. An accepted
transaction finishes within its bounded transfer window. The daemon never
automatically retries a storage write or reboots the device.

After an interrupted upload, choose **Clear recovery and retry playback** in
Installation Health or restart the daemon. Disconnecting the screen for at least
20 seconds while the daemon runs also clears recovery. These actions retry
normal playback without repeating the upload. If the screen remains
unresponsive, power-cycle it. Recovery remains available for earlier failed
uploads, including devices whose startup-image support has been removed.

JPEG payloads are limited to 101,888 bytes. Images are decoded and re-encoded
at the panel's native size. Choose a simpler image if it exceeds the limit.
SL/TL wireless LCDs use the vendor's legacy startup path when the revision probe
has no reply.

Flex uploads require the matching LCD receiver on the same USB hub. The daemon
selects the physical panel's startup-image mode, preserves sibling panels'
embedded-theme selection, saves that receiver setting, and transfers the
400 × 400 JPEG. The image travels over USB even when the fans are bound
wirelessly. Missing or ambiguous receiver topology prevents storage writes.

Embedded startup-theme selection and autonomous startup video are not part of
this feature. An SDK method or command enum alone is not sufficient to enable
a new startup-content path.
