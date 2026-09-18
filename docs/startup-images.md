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

HydroShift II Circle and Square also have an **experimental wireless upload**
on the LCD page. Bind the H2 to the wireless controller first. Keep its USB
connection in place. This path uses the receiver's
image-transfer commands and limits the JPEG to 20 KiB. Quality is adjusted
automatically. Cooling control continues during upload. The daemon matches the
USB display to the receiver using the H2-reported MAC address, pauses that
display's media, then restores playback afterward. Missing or ambiguous MAC
association prevents the upload without affecting unrelated displays.
Use LCD mode rather than Desktop mode for this operation.

The H2 firmware saves this image as `aio.jpg`, but automatic display after a
power cycle is not verified. The action reports receiver acknowledgement,
not verified storage or boot persistence. It does not replace `boot.jpg` or
the factory logo. Check host exit and a complete screen power cycle before
relying on the result. Cancellation or a missing acknowledgement stops the
transfer without submitting its completion packet. An already submitted
completion packet may still save the image.

Universal Screen 8.8, Lancool 207, Vision 9.2 and
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

USB startup JPEG payloads are limited to 101,888 bytes. Images are decoded and re-encoded
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
