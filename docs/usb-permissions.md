# USB permissions

Open **Installation Health** in the GUI to inspect the installed `60-lianli.rules` and
recheck after repairs. The popup can be dismissed with **Later**; the page remains available.
The checks never change rules, trigger devices or start a daemon.

## Native installation

Packages supply `/usr/lib/udev/rules.d/60-lianli.rules`. For a source installation, run
these commands from the repository:

```sh
sudo install -Dm644 packaging/udev/60-lianli.rules /usr/lib/udev/rules.d/60-lianli.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

The active desktop user normally receives access through `uaccess`. The system daemon
uses the `lianli` group created by the package's sysusers rule. Running the GUI as root
is not required. If the system account is missing, see [service setup](service-modes.md).

## Missing, changed or disabled rules

The GUI compares active rule lines with the version bundled into the application.
Comments and blank lines do not affect the comparison. A difference is **unverified**,
not proof of a permission failure: intentional custom rules can provide valid access.

A same-named file in `/etc/udev/rules.d` overrides `/run/udev/rules.d` and vendor copies.
An empty override or a symlink to `/dev/null` disables the packaged rules. Review local
overrides before replacing them; an old copied rule can keep shadowing package updates.
Files with other names can also alter the final permissions. See
[systemd's rule precedence documentation](https://github.com/systemd/systemd/blob/main/man/udev.xml).

A matching file does not prove that udev has reloaded it, that attached nodes have received
the rule, or that a different daemon account can access them. If devices remain missing,
check the selected service journal for permission errors using
[troubleshooting](troubleshooting.md). A device that is absent is not itself evidence of
denied access.

Installation Health checks USB/HID node permissions in the connected daemon's
account and mount namespace, using its startup HID backend. Bulk devices and the
`rusb` backend require `/dev/bus/usb` nodes; `hidraw` devices require nodes at the
same USB topology. The check includes the V2 dongle's HID companion. A missing node
is unverified, with binding/container visibility guidance. No visible supported
devices is a separate result, not permission denial.

The kernel's [effective-access check](https://man7.org/linux/man-pages/man2/access.2.html)
evaluates read/write permissions, including ACLs. Node metadata is pinned with
[`O_PATH`](https://man7.org/linux/man-pages/man2/open.2.html) and its character-device
number is compared with sysfs; the USB/HID driver's open operation is not invoked.
This does not prove that interface claims, device cgroup policies or later driver
operations will succeed. A device with multiple HID interfaces and mixed permissions
remains unverified because metadata alone does not identify the required usage page.

When all checked devices pass, a missing/customized packaged rule file becomes
informational and the GUI does not ask you to replace working custom rules. This
requires a daemon result; desktop-user access cannot establish system-daemon access.
Hidden nodes, ambiguous interfaces or no visible devices do not suppress the warning.
Checks are bounded and run on request, not with normal telemetry polling.

Shared-lock checks also report stale numeric group membership without acquiring
the lock. See the [runtime diagnostic command](troubleshooting.md) for offline
user-service setup and explicit backend selection.

## Distrobox

Install rules and group membership **on the host**, which owns the USB device nodes.
Installing the package inside the box alone is insufficient. Follow the
[Distrobox guide](distrobox.md), including logout/login and restarting a still-running box.
Installation Health reads host rules through `/run/host`; an unavailable host is shown
as unverified, never as a successful check of the box's private rules.
