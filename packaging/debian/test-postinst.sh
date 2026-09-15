#!/usr/bin/env bash
set -euo pipefail

source_script=$(realpath "${1:-$(dirname "$0")/../../debian/lian-li-linux.postinst}")
stage=$(mktemp -d "${TMPDIR:-/tmp}/lianli-postinst-test.XXXXXX")
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/bin" "$stage/run/udev" "$stage/run/systemd/system"
sed -e "s|/run/udev|$stage/run/udev|g" -e "s|/run/systemd/system|$stage/run/systemd/system|g" "$source_script" > "$stage/postinst"
cat > "$stage/bin/mock" <<'SH'
#!/bin/sh
name=${0##*/}
printf '%s\n' "$name $*" >> "$CALLS"
case "$name" in
    systemd-detect-virt) exit "$CONTAINER_STATUS" ;;
    udevadm) exit "$RELOAD_STATUS" ;;
    systemctl) exit "$SYSTEM_RELOAD_STATUS" ;;
esac
SH
chmod +x "$stage/bin/mock"
for command in systemd-sysusers systemd-tmpfiles systemd-detect-virt udevadm systemctl; do
    ln -s mock "$stage/bin/$command"
done
export PATH="$stage/bin:$PATH" CALLS="$stage/calls"
export CONTAINER_STATUS=0 RELOAD_STATUS=1 SYSTEM_RELOAD_STATUS=0
sh "$stage/postinst" configure
! grep -q '^udevadm ' "$CALLS"
! grep -q '^systemctl ' "$CALLS"
grep -q '^systemd-sysusers ' "$CALLS"
grep -q '^systemd-tmpfiles ' "$CALLS"

: > "$CALLS"
export CONTAINER_STATUS=1 RELOAD_STATUS=0
sh "$stage/postinst" configure
grep -qx 'udevadm control --reload-rules' "$CALLS"
grep -qx 'systemctl --system daemon-reload' "$CALLS"
test "$(grep -c '^systemctl ' "$CALLS")" = 1

export SYSTEM_RELOAD_STATUS=1
if sh "$stage/postinst" configure; then
    echo 'Native system manager reload failure was ignored.' >&2
    exit 1
fi
export SYSTEM_RELOAD_STATUS=0

export RELOAD_STATUS=1
if sh "$stage/postinst" configure; then
    echo 'Native udev reload failure was ignored.' >&2
    exit 1
fi

rmdir "$stage/run/udev"
: > "$CALLS"
sh "$stage/postinst" configure
! grep -q '^udevadm ' "$CALLS"
grep -qx 'systemctl --system daemon-reload' "$CALLS"
rmdir "$stage/run/systemd/system"
: > "$CALLS"
sh "$stage/postinst" configure
! grep -q '^systemctl ' "$CALLS"
: > "$CALLS"
sh "$stage/postinst" abort-upgrade
test ! -s "$CALLS"
printf '%s\n' 'Post-install native/container udev and system manager behavior passed.'
