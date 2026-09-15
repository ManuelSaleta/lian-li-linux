# Hardware video acceleration

Open **Settings → Configuration → Hardware video acceleration**, choose the setting,
then **Save**. It is disabled by default, including when upgrading an existing configuration.
The persisted field is `hardware_video`. The old `LIANLI_ENABLE_HW_VIDEO` environment
variable is no longer used.

When disabled, video decoding and encoding use software. This includes desktop-mode
H.264 encoding, which previously tried GPU encoders regardless of the environment variable.
The compositor may still use the GPU to render the desktop.

When enabled, the daemon tries available hardware encoders before software H.264.
Video decoding uses FFmpeg's automatic hardware selection. Drivers and FFmpeg support
determine what can run; enabling this setting does not guarantee GPU acceleration or
zero-copy capture. Desktop encoding currently tries NVENC and AMF before libx264.

Saving applies to file video, template video widgets, live sensor/custom H.264 streams,
and desktop capture. File/template preparation runs in the background, and newer saves
cancel obsolete work. Installation Health reports preparing, prepared or failed for each configured
LCD. Disconnected LCDs wait until discovery resolves their physical screen capabilities;
connecting the panel starts preparation without another Save. Prepared means the asset is ready; the panel still needs to accept
playback. A failed replacement keeps existing content where available. Save again to retry.

H.264 uses whole-number frame rates. File transcodes and live sensor/custom streams
round fractional limits down, matching desktop encoding. The rate remains within
the panel's supported range, with a minimum of 1 FPS. Live rendering, encoder timing
and USB delivery use the same rate.

There is at most one preparation worker and one pending replacement batch. Cancellation
reaches FFmpeg, metadata probes and animation/template decoding between frames or widgets.
Each asset has a three-minute preparation deadline; an individual file read or image decode
cannot be interrupted mid-call. Pixel cleaning waits until preparation has finished.
Hotplug-triggered preparation waits for active pixel cleaning to finish.

Live H.264 startup retains at most eight diagnostic lines, capped at 2 KiB of
input per line. Runtime encoder warnings are limited to one every five seconds
and include the number suppressed since the last report. Excessively long lines
are drained without retaining their full text, and a continuous diagnostic flood
is throttled independently of normal frame delivery.
Failed live-encoder startup terminates and reaps its FFmpeg child before trying
another encoder, including failures to start the diagnostic reader thread.
Live encoder startup validates the rotated dimensions against the H.264 panel
and limits each input RGBA frame to 64 MiB before allocating or launching FFmpeg.
File transcodes must produce nonempty output within the temporary-file limit.
An empty result fails preparation and preserves the previous working asset.

Desktop capture keeps its virtual
monitor and USB connection while replacing the encoder. Panels that require JPEG keep
using their existing JPEG protocol. Static images and pixel conditioning do not need GPU
video acceleration.

If playback fails, disable the setting and save again. Inspect the active daemon's logs:

```sh
journalctl --user -u lianli-daemon -b
# For the system service:
sudo journalctl -u lianli-daemon-system -b
```

Encoder messages identify the selected implementation and failures. A system daemon needs
access to the GPU render node under `/dev/dri`; access in your login session alone does not
prove the service can use it. In Distrobox, GPU libraries and render nodes must be available
inside the box, while the GPU driver runs on the host.

If the switch is unavailable while connected, update and restart the daemon so it advertises
support for this setting.
