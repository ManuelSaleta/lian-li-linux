#!/usr/bin/env bash
set -euo pipefail

checker=$(realpath "$(dirname "$0")/check-startup-links.sh")
stage=$(mktemp -d "${TMPDIR:-/tmp}/lianli-startup-test.XXXXXX")
trap 'rm -rf "$stage"' EXIT
user_units="$stage/usr/lib/systemd/user/default.target.wants"
system_units="$stage/usr/lib/systemd/system/multi-user.target.wants"
mkdir -p "$user_units" "$system_units"
ln -s ../lianli-session.service "$user_units/lianli-session.service"
ln -s ../lianli-control-recovery.service "$system_units/lianli-control-recovery.service"
bash "$checker" "$stage"

reject() {
    if bash "$checker" "$stage" > "$stage/output" 2>&1; then
        echo "Startup check accepted an unexpected service: $1" >&2
        exit 1
    fi
}

for mode in user system; do
    if [ "$mode" = user ]; then
        link="$user_units/lianli-daemon.service"
    else
        link="$system_units/lianli-daemon-system.service"
    fi
    ln -s ../lianli-daemon.service "$link"
    reject "$mode hardware daemon"
    rm "$link"
done
rm "$user_units/lianli-session.service"
ln -s ../lianli-daemon.service "$user_units/lianli-session.service"
reject 'capture helper redirected to hardware daemon'
rm "$user_units/lianli-session.service"
ln -s ../lianli-session.service "$user_units/lianli-session.service"
ln -s ../unrelated.service "$user_units/unrelated.service"
reject 'unrelated startup unit'
rm "$user_units/unrelated.service"
bash "$checker" "$stage"
printf 'Package startup-link regression checks passed.\n'
