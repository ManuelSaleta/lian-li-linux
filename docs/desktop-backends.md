# Desktop display backends

The desktop backend redesign is in progress. The session worker running in the
active user's Hyprland session prefers native headless output and CPU capture. Native setup
failure is reported rather than falling back to a DRM output or physical-monitor
capture. Other sessions attempt Hermes-KMS first, then EVDI after a failed Hermes
attempt has released its owned output. Healthy outputs retain their backend.
The integrated paths still require the release hardware/compositor matrix.

Hyprland's headless backend advertises a default `1920x1080@60` mode even when
the active custom mode is different. For a Universal Screen 8.8, the active
`1920x480@60` line is the relevant capture size. `availableModes` is not the
current resolution. The application sets and verifies a custom mode through
Wayland output management. Tools that only accept advertised modes can reject
that size despite it being active. Changing the advertised list requires support
in the compositor/backend; the current headless creation API does not accept an
EDID or mode list. See the [upstream headless backend](https://github.com/hyprwm/aquamarine/blob/main/src/backend/Headless.cpp).

`lianli-session` controls and captures the desktop, then encodes frames for the
selected hardware daemon. Ordinary fan, RGB and LCD media operation requires only
the hardware daemon; this helper is required only for desktop mode.
The helper uses its own `XDG_RUNTIME_DIR`,
`WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE`. Do not copy another user's
environment into a system service. Native packages start `lianli-session.service`
with the user manager, including on Hyprland setups without XDG autostart. It waits
for an active graphical login; opening the GUI is not required. The helper reads
bounded process-environment snapshots owned
by that user and matching the active login ID. It requires an existing owned
Wayland socket. For Hyprland, it also verifies a live control connection and matching
compositor credentials before accepting a candidate. Stale environments are skipped,
and conflicting live candidates are refused. Only the selected desktop
fields are retained and passed to capture; unrelated environment variables are not copied.
The packaged GUI and XDG autostart entry request the same user service. Closing
the GUI leaves capture running. Duplicate direct launches in the same login exit.

This also applies when launching `target/release/lianli-daemon` manually: run the
matching `target/release/lianli-session` from a terminal in your graphical session,
or launch the matching GUI, which starts it automatically. After rebuilding,
restart the session helper as well as the daemon so capture uses the new code.
New supervisors detect replacement of their executable within five seconds,
stop their capture child, recover owned outputs, and restart using the updated
binary. Supervisors predating this behavior need a one-time manual restart;
reopening the GUI does not replace an already running older supervisor.
For the native system service, keep this helper running as your desktop user;
it connects to the system daemon automatically. The system daemon needs no
`HYPRLAND_INSTANCE_SIGNATURE` or `WAYLAND_DISPLAY` overrides. The helper selects
the exact Hyprland instance through its signature and verifies that its control
and Wayland sockets belong to the same compositor process.

The launcher supervises one capture child for that login session, independently
of the GUI. It checks the Wayland socket identity every five seconds. If the socket
disappears or is replaced, it stops capture and rediscovers the active desktop before
restarting. An abnormal child exit restarts
capture with a delay of 1–30 seconds; a minute of successful operation resets the
delay. Logout or launcher termination stops capture, allowing ten seconds for
cleanup before forced termination. Killing the launcher also kills its capture
child, preventing orphaned output owners. This supervisor does not restart the
hardware daemon or change its selected service mode.

Capture jobs report progress even while idle. A job making no progress for twenty
seconds, or failing to stop within ten seconds after cancellation, terminates
the capture child so a stuck GPU call cannot indefinitely block recovery. This
restarts all displays in that capture process. The supervisor checks recorded
Hyprland outputs after child exit and reclaims abandoned outputs before restart.

Hyprland output ownership is recorded in a private directory beside that
compositor instance's control socket. Recovery skips records locked by live
workers and checks the compositor and output identities before removing an
abandoned output. Unrelated headless monitors are preserved. A replaced or
invalid ownership record stops recovery rather than authorizing broad cleanup.

For a source build, build the workspace so `lianli-session` sits beside
`lianli-gui`. To start it without the GUI, add the absolute path to
`target/release/lianli-session` to the compositor's login commands (Hyprland:
`exec-once = /absolute/build/path/target/release/lianli-session`), or install a user
unit whose `ExecStart` points to that binary with `--login-start` and enable it for
`default.target`. Building binaries alone does not install login startup.
Native package users can inspect `systemctl --user status lianli-session.service`
and `journalctl --user -u lianli-session.service`; mask the user service to opt out
of automatic desktop capture. No hardware daemon service is enabled by this setup.
A Distrobox GUI starts the helper in
the same box; it needs the host system bus at `/run/host/run/dbus/system_bus_socket`
and uses the user-daemon route. System mode requires the native package.

Both ends authenticate Unix peer credentials. The daemon accepts one worker for
the active local graphical session on `seat0`; remote, greeter and unknown
sessions cannot capture. Locked sessions pause capture and power down the panel;
unlock starts with a fresh frame and a recreated H.264 encoder. Session removal
stops its helper, and daemon/worker connection loss tears down owned outputs.
Logind's lock state is a compositor-provided hint; capture denial also fails
closed. Logind unavailability prevents desktop capture while cooling, RGB and
ordinary LCD playback continue. A newly registered session resets desktop startup
backoff. Multi-seat selection is not currently supported.

Control uses versioned Unix sequenced packets with bounded deadlines. Each
display has one reusable encoded buffer, limited to 8 MiB and sealed against
resizing. The daemon requests another frame only after consuming the previous
one; policy generations discard obsolete results. The helper has no USB access
in its code and supports at most 16 simultaneous displays. Helper and daemon
versions must match. Full install/session/compositor acceptance remains pending.

The `lianli-display` crate owns virtual output and capture operations. USB control
remains in the selected hardware daemon. Frames carry explicit dimensions,
refresh rate, pixel format and stride; modes must be advertised by the physical
panel and CPU frame storage is limited to 64 MiB. Encoding accepts padded rows.

CPU and GPU capture frames retain source timestamps when available. Hermes uses
the host monotonic clock and the latest desktop/cursor update in the composed
frame. Hyprland retains its compositor-provided timestamp, labeled with a separate
clock domain. EVDI timestamps remain unknown. These values are not wall-clock
times and must not be compared across clock domains. Frame pacing still follows
the configured delivery limit.

Capture frames also distinguish unknown damage, a full refresh, unchanged pixels,
and a bounding rectangle of changed pixels. Rectangles use half-open bounds in
visible frame coordinates. Hermes accumulates damage across acquisition retries
and requests a full refresh for cursor changes. Hyprland combines damage events
until capture completes and uses a full refresh for its first or vertically
inverted frame. EVDI damage remains unknown. This metadata does not change the
current whole-frame encoding and USB delivery paths.

Opening a capture backend does not prove that it can display an encoded frame.
While active capture is requested, the session worker allows ten seconds for the
first encoded frame before reporting failure. This check also applies after a
mode change, resume or encoder-policy change. Once a frame has been delivered,
an unchanged desktop can wait for damage without this startup timeout.

Desktop → LCD switching stops capture asynchronously and reserves the selected
USB attachment until the switch finishes. The firmware command runs only after
capture releases the device. Other service work continues during teardown and
the device's settling delay. Shutdown cancels a pending switch or waits for an
already-started command before tearing down hardware access.
LCD → Desktop switching likewise transfers the selected LCD to the worker before
stopping its media and issuing the firmware command. If no LCD target is active,
the worker opens only the selected device. The target stays suppressed from
automatic reopening until the operation completes.

Settings → Configuration → Max FPS Limit also caps desktop encoding and USB
delivery. The effective rate is the lower of that limit and the negotiated
display rate. Fractional limits round down to a whole encoding rate. FPS and
hardware-video changes recreate the H.264 encoder without recreating the output;
JPEG desktop devices use the same delivery pacing. Delayed frames do not build
up a queue to send later.

Desktop H.264 uses a peak bitrate limit and a two-frame rate-control buffer on
both CPU-input and VAAPI encoders. This bounds high-motion bursts instead of
leaving NVENC's default two-second buffer. The average bitrate target is unchanged.
Software encoding retains CRF 23 with the same burst limits, so very complex
motion may trade detail for lower transfer pressure. This limits encoder bursts,
not the panel's internal decode queue. Sustained 60 FPS latency still requires
physical validation. Ordinary LCD media encoding and JPEG are unaffected.

## Hyprland interface baseline

The native integration targets Hyprland 0.47 and newer with named headless
outputs, output management and a supported capture protocol. It verifies the
running session through its control socket and peer credentials, then creates
a unique output name. Cleanup checks that session and output identity still
match. It never removes all headless outputs or selects a physical monitor as
a capture fallback.

The CPU capture implementation negotiates `wl_output` version 4,
`zwlr_output_manager_v1` version 4 and `zwlr_screencopy_manager_v1` version 3.
It uses output management to set the exact panel mode at scale 1 and verifies
the accepted geometry/refresh. This Hyprland-specific path submits only the owned
head: the checked Hyprland implementations accept partial configurations but do
not enforce the configuration serial. It must not be reused as a generic Wayland
output-configuration client. A compositor that rejects partial configuration
fails setup and cleans up the owned output.

Position is controlled by Hyprland's monitor rules and automatic layout. The
capture connection does not set a Wayland position override, because Hyprland
would give that override priority over subsequent native layout changes.

Capture composites the cursor, requests a full initial frame and then waits for
damage. One capture is outstanding at a time. Shared capture storage is an
anonymous memfd sealed against resizing; CPU playback uses a separate stable
buffer. Each is bounded to 64 MiB. Padded rows and vertically inverted buffers are
handled explicitly. This establishes the CPU path; DMA-BUF acceleration remains
separate work.

Control requests use Hyprland's Unix IPC directly, with two-second deadlines
and replies capped at 512 KiB. They run only during setup, explicit checks and
teardown; frame capture uses the session's event-driven Wayland protocol.
The initial version request uses the same connection as the peer-credential check;
an idle probe connection would block Hyprland's serial request handler.

Interface references: [Hyprland IPC](https://wiki.hypr.land/IPC/),
[named output creation](https://wiki.hypr.land/0.47.0/Configuring/Using-hyprctl/),
and the compositor's [control implementation](https://github.com/hyprwm/Hyprland/blob/main/src/ipc/s1/Commands.cpp).

## Optional kernel modules

Hyprland's native headless backend needs neither Hermes-KMS nor EVDI. For other
desktops, install the optional driver on the host using its supported package or
upstream instructions. A Distrobox shares the host kernel. Installing a module
only inside the box does not set up the host driver.

Installation Health checks the host kernel's loaded modules, installed module
files and DKMS registration. When a registered module is missing, it checks for
matching kernel build files. It also examines up to 80 relevant kernel messages
from the current system boot for load and signature rejection errors. Distrobox
uses the host command bridge for these checks. Missing tools or inaccessible logs
are reported as unverified. No module is loaded by a check.

An absent optional module is N/A. A registered but missing module needs its host
build/install completed. A loaded module supersedes earlier load errors, but still
needs successful output setup and capture. These checks do not inspect every
third-party installer or establish that a signing key is trusted before loading.

Check the running kernel with `uname -r`. For a DKMS installation, `dkms status`
shows whether the driver is added, built or installed for that kernel. A source
build needs matching kernel headers/build files, normally exposed through
`/lib/modules/$(uname -r)/build`. Follow your host distribution's instructions
for the matching kernel development package. An already installed binary module
does not need headers just to run. See the [DKMS manual](https://github.com/dkms-project/dkms/blob/master/dkms.8.in).

| Symptom | Next check |
| --- | --- |
| Driver missing after a kernel upgrade | Check its installation for the running kernel, not just the previous kernel. |
| DKMS build failed | Read the package's build output and DKMS build log under `/var/lib/dkms`. Check matching headers and driver support for that kernel. |
| Module installed but unavailable | Read the host kernel journal with `journalctl -k -b`. Check the module load error before retrying desktop mode. |
| Signature or key rejected | Follow your distribution's module signing and trusted-key enrollment procedure. |
| Module loaded but desktop startup fails | Check Installation Health for permissions, driver UAPI, output geometry, compositor activation and capture errors. Loading alone does not establish a usable backend. |

Where available, `mokutil --sb-state` reports Secure Boot status. A module's
signer field (for example, `modinfo -F signer evdi`) identifies its signer but does not prove the running
kernel trusts that key. Signing and enrollment depend on the host's boot setup.
See [DKMS signing and Secure Boot guidance](https://github.com/dkms-project/dkms#module-signing)
and the [kernel's signature enforcement rules](https://docs.kernel.org/admin-guide/module-signing.html).

The app does not change boot security, enroll keys or unload a live display
module. Repair the host installation first, then retry desktop mode. When
reporting a module problem, include the relevant build or kernel errors alongside
the app's diagnostic export and daemon logs. Review those extra logs before sharing.

## Hermes-KMS interface baseline

The implementation is pinned to Hermes-KMS commit
`7fbdd011cc74a57c52e1beea0f98c10c5fd5f198`, UAPI 13. Other UAPI
versions fail the compatibility check. Driver/module installation is separate
from installing this application.

Only render nodes identifying themselves as `hermes-kms` through the DRM core
version query receive private ioctls. The implementation accepts host/general
devices, rejects private-session devices and claims only an idle output.
Dynamic device indices are not assumed to be dense or stable. Ownership is tied
to the open descriptor and kernel session; closing it revokes that ownership.
No DRM-master acquisition or module topology changes are used.

Installation Health checks effective read/write permission on visible Hermes host
render nodes using the desktop process credentials, including ACL masks. This is
a metadata-only check and does not open a capture session. Private-session nodes
are excluded. A Distrobox check reflects the nodes and credentials visible inside
that box.

For denied host access, follow [Hermes permissions](troubleshooting.md#hermes-permissions).
The supplied `60-lianli.rules` includes a targeted ACL-mask workaround. It matches
unassigned host nodes and the legacy static general node by their sysfs identity,
not a fixed render-node number. Private sessions and explicitly assigned nodes
retain the driver's access policy. Root ownership/group and active-seat ACLs are
preserved. A final `0660` mode keeps the mask writable despite later driver rules
setting `0600`; it does not grant the `render` or `lianli` groups access.

The workaround addresses the interaction between the driver's mode reset and
[systemd's ACL update](https://github.com/systemd/systemd/blob/main/src/shared/acl-util.c),
which can skip recalculating the mask when the permitted user entry is unchanged.
Login/logout still manages the named user ACL. Recheck permissions after driver
upgrades or changes to its ownership policy.

The preferred physical-panel mode must fit the module's reported limits.
Upstream defaults to a minimum width of 640 and height of 480; a 480-pixel-wide
panel therefore needs suitable module settings or the EVDI fallback.
The application will not stretch the mode or rewrite module settings. The host
compositor must receive hotplug events and scan out the requested mode; startup
verifies actual geometry and a captured frame before selecting Hermes.

The readback path needs `libEGL.so.1`, `libgbm.so.1`, a working GPU driver with
OpenGL ES 3, DMA-BUF import/modifier support and surfaceless contexts. It requires
external-image sampling support when a GPU advertises external-only modifiers.
Both CPU readback and GPU cursor composition use the matching sampler type.
The EGL context does not require pbuffer support on the GBM device. Physical
format/modifier compatibility still depends on the compositor and GPU driver.
It waits
for both frame and cursor synchronization fences, samples imported images into
owned GPU textures, and reads those textures through reusable buffers. It never
CPU-maps the compositor's scanout allocation. Software rasterizers and unsupported
formats/modifiers cause fallback. Desktop and cursor allocations are reused;
cursor-only movement does not trigger a full desktop readback, and known empty
frame damage does not cause another encoded frame. GPU waits and acquisition
retries are bounded, with cancellation reaching capture and readback.

Hardware-video disabled uses software video encoding. A GPU copy used for safe
capture readback is separate from video encoding/decoding. JPEG-only panels keep
their JPEG path regardless of this setting.

For Hermes H.264 output with hardware video enabled, capture composites desktop
and cursor into a reusable linear RGB GBM allocation, waits for GPU completion,
then passes its DMA-BUF to VAAPI on the same render device. GPU conversion through
`libavfilter` produces NV12 for H.264 encoding. Startup verifies CPU readback once;
successful GPU encoding then avoids CPU pixel readback. Only linear allocations
are accepted, avoiding the legacy PRIME import fallback that can omit tiling modifiers in
[FFmpeg's VAAPI importer](https://ffmpeg.org/doxygen/7.1/hwcontext__vaapi_8c_source.html).

A failed GPU allocation, import, conversion or encode releases the encoder and
target, then falls back to CPU readback and the existing encoder selection.
Failure of a CPU-input hardware encoder falls back to libx264. Failed paths are
not retried per frame; a policy change or capture restart permits another attempt.
The capture backend and Hermes fallback reason reach the daemon; encoder changes
are recorded in the helper journal. Hyprland capture remains CPU-based. Actual
colors, cursor placement, GPU compatibility and performance still require the
hardware matrix; this implementation does not establish universal zero-copy support.

Protocol source: [pinned public header](https://github.com/MrOz59/Hermes-KMS/blob/7fbdd011cc74a57c52e1beea0f98c10c5fd5f198/include/uapi/drm/hermes_kms_drm.h),
[driver behavior](https://github.com/MrOz59/Hermes-KMS/blob/7fbdd011cc74a57c52e1beea0f98c10c5fd5f198/kernel/hermes-kms/hermes_kms.c).
The public ABI declarations retain their GPL-2.0 with Linux-syscall-note notice;
the independent userspace implementation remains MIT-licensed.
