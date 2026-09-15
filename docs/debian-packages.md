# Debian and Ubuntu packages

The release build matrix targets native amd64 Ubuntu 24.04, Ubuntu 26.04 and
Debian 13 separately. These remain release candidates until the corresponding
build, installation and desktop validation have passed. Use the package matching
your distribution and release; FFmpeg, WebKit and libc dependencies differ.

Download the matching `.deb` and `SHA256SUMS` from the GitHub release. Verify the
download against its entry, then install it with APT so dependencies are resolved:

```sh
sha256sum --ignore-missing --check SHA256SUMS
sudo apt install ./lian-li-linux_VERSION+DISTRIBUTION_amd64.deb
```

The package contains the daemon, GUI, udev rules, both service units, icons and
focused guides under `/usr/share/doc/lian-li-linux/guides`. Neither hardware
service is enabled on a fresh install. Open the GUI for installation checks and
follow [service modes](service-modes.md) to choose one owner.

Package upgrades preserve the selected startup mode and leave the running daemon
in place. Restart the selected daemon cleanly after upgrading so it uses the new
binary. Rules are reloaded during installation; existing devices may require a
reconnect or a new login as explained in [USB permissions](usb-permissions.md).

EVDI is optional. Its library and host kernel module are needed only for that
desktop backend; they are not installed automatically. Distrobox users must keep
kernel modules and effective USB rules on the host; see [Distrobox](distrobox.md).

Removal and purge retain user-authored configuration and media, including state
under `/var/lib/lianli` and home directories. Stop the selected user service before
removing the package. Purge removes packaged units and debhelper bookkeeping. It
retains `/etc/lianli/service-selection.json`, including an unfinished switch gate,
and the shared ownership/operation locks: another installation may still use
them. Reinstallation retains this selection without automatically enabling a
hardware service. Use the service-management UI to select a different owner or
recover an unfinished switch; deleting this record would bypass that protection.

## Building

Use a disposable container or VM matching the target distribution. From a full
checkout with submodules, install build dependencies inside that environment:

```sh
bash packaging/debian/install-build-deps.sh
```

Use a current stable Rust toolchain and Node 22. The distribution's Rust package
may be older than the versions required by the locked dependencies. The build
uses conventional debhelper packaging and generates ELF runtime dependencies with
`dh_shlibdeps`; subprocess tools are declared separately.

The existing Rust JPEG encoder uses TurboJPEG 3 APIs, while these distribution
releases ship TurboJPEG 2. The package therefore retains the application's
Cargo-locked bundled JPEG library, includes its license notices, and records a
specific Lintian exception for it. FFmpeg, WebKit, libc and other linked system
libraries still use generated distribution dependencies.

```sh
bash packaging/debian/build.sh /path/to/artifacts
```

The script stages a copy of the checkout, builds and tests it, runs package checks
and Lintian, and writes the `.deb`, `.buildinfo` and `.changes` files to the output
directory. It retains the temporary build tree for inspection. Source files in
the checkout are not changed. The isolated build needs network access for locked
Cargo/npm dependencies unless their caches are already populated.

The **Debian packages** GitHub workflow offers artifact-only builds. The existing
**Release Notes** workflow still supports non-publishing notes previews; tag
publishing now waits for all three package builds and installation smoke checks,
then attaches the distribution packages and their combined `SHA256SUMS` file.
Desktop/hardware validation is a separate release acceptance step.

Package smoke checks cover fresh installation, a synthetic lower-version upgrade,
reinstallation, removal and purge. They verify preservation of configuration,
profiles, startup selection, private state permissions and lock identity. The
upgrade fixture changes package version metadata while keeping the payload, so
it checks maintainer-script upgrade behavior rather than compatibility with a
historical release. Tests run in disposable containers without systemd or devices.
