#!/usr/bin/env bash
set -euo pipefail

package=$(realpath "$1")
[ "$(dpkg-deb -f "$package" Package)" = lian-li-linux ]
[ "$(dpkg-deb -f "$package" Architecture)" = amd64 ]
dependencies=$(dpkg-deb -f "$package" Depends)
for required in libc6 ffmpeg fontconfig systemd udev libwebkit2gtk-4.1-0; do
    if ! [[ ",$dependencies," =~ [,[:space:]]$required[[:space:],\(] ]]; then
        echo "Package is missing dependency $required: $dependencies" >&2
        exit 1
    fi
done
if [[ "$dependencies" == *evdi* ]]; then
    echo "EVDI must remain an optional dependency." >&2
    exit 1
fi
stage=$(mktemp -d "${TMPDIR:-/tmp}/lianli-deb-check.XXXXXX")
dpkg-deb -R "$package" "$stage"
for binary in lianli-daemon lianli-gui lianli-session lianli-control; do
    [ -x "$stage/usr/bin/$binary" ]
    [ -f "$stage/usr/share/man/man1/$binary.1.gz" ]
    dynamic=$(readelf -d "$stage/usr/bin/$binary")
    if grep -q 'NEEDED.*libevdi' <<< "$dynamic"; then
        echo "$binary still requires libevdi." >&2
        exit 1
    fi
done
control_dynamic=$(readelf -d "$stage/usr/bin/lianli-control")
if grep -Eq 'NEEDED.*lib(usb|hidapi|webkit|gtk|avcodec|avfilter|EGL|gbm|evdi)' <<< "$control_dynamic"; then
    echo "Standalone diagnostics must not require hardware, media or GUI libraries." >&2
    exit 1
fi
[ -f "$stage/usr/lib/udev/rules.d/60-lianli.rules" ]
[ -f "$stage/usr/lib/systemd/user/lianli-daemon.service" ]
[ -f "$stage/usr/lib/systemd/user/lianli-session.service" ]
[ "$(readlink "$stage/usr/lib/systemd/user/default.target.wants/lianli-session.service")" = ../lianli-session.service ]
[ -f "$stage/etc/xdg/autostart/com.sgtaziz.lianlilinux.session.desktop" ]
[ -f "$stage/usr/lib/systemd/system/lianli-daemon-system.service" ]
[ -f "$stage/usr/lib/systemd/system/lianli-control-recovery.service" ]
[ -f "$stage/etc/xdg/autostart/com.sgtaziz.lianlilinux.recovery.desktop" ]
[ "$(readlink "$stage/usr/lib/systemd/system/multi-user.target.wants/lianli-control-recovery.service")" = ../lianli-control-recovery.service ]
node "$(dirname "$0")/../polkit/test-recovery-rule.cjs" "$stage/usr/share/polkit-1/rules.d/49-lianli-recovery.rules"
[ -f "$stage/usr/share/doc/lian-li-linux/guides/usb-permissions.md.gz" ] || \
    [ -f "$stage/usr/share/doc/lian-li-linux/guides/usb-permissions.md" ]
bash "$(dirname "$0")/check-startup-links.sh" "$stage"
appstreamcli validate --no-net "$stage/usr/share/metainfo/com.sgtaziz.lianlilinux.metainfo.xml"
node "$(dirname "$0")/../desktop/check-metainfo.cjs" \
    "$stage/usr/share/metainfo/com.sgtaziz.lianlilinux.metainfo.xml" "$stage/usr/lib/udev/rules.d/60-lianli.rules"
lintian --fail-on error "$package"
printf 'Inspected package contents: %s\n' "$stage"
