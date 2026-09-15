set -eu
PATH=/usr/bin:/bin
export PATH
umask 077

fail() { echo "$1" >&2; exit 1; }
[ "$(id -u)" = 0 ] || fail 'Host setup requires administrator authorization'
[ "$#" = 4 ] || fail 'Invalid host setup arguments'
caller_uid=$1
expected_digest=$2
source_binary=$3
deployment=$4
case "$caller_uid" in ''|0|*[!0-9]*) fail 'Invalid host setup account';; esac
[ "${PKEXEC_UID:-}" = "$caller_uid" ] || fail 'Host setup authorization belongs to another account'
caller_gid=$(id -g -- "$caller_uid")
[ "${#expected_digest}" = 64 ] || fail 'Invalid host helper digest'
case "$expected_digest" in *[!0-9a-f]*) fail 'Invalid host helper digest';; esac
case "$source_binary" in /run/user/"$caller_uid"/lianli-host-*/lianli-control) ;; *) fail 'Invalid staged host helper path';; esac
stage_name=${source_binary#/run/user/"$caller_uid"/}
stage_name=${stage_name%/lianli-control}
case "$stage_name" in */*|*..*) fail 'Invalid staged host helper directory';; esac
for utility in stat dirname realpath mkdir mktemp timeout setpriv head sha256sum chmod sync mv rm; do
    command -v "$utility" >/dev/null || fail 'Install coreutils and util-linux on the host before setup'
done

check_tree() {
    checked_path=$1
    while :; do
        [ "$(stat -c %u -- "$checked_path")" = 0 ] || fail 'Host helper path is not root-owned'
        if [ ! -L "$checked_path" ]; then
            checked_mode=$(stat -c %a -- "$checked_path")
            [ "$((0$checked_mode & 022))" = 0 ] || fail 'Host helper path is writable by another account'
        fi
        [ "$checked_path" != / ] || break
        checked_path=$(dirname -- "$checked_path")
    done
}

for directory in /usr /usr/local /usr/local/libexec /usr/local/libexec/lianli; do
    if [ ! -e "$directory" ] && [ ! -L "$directory" ]; then
        check_tree "$(dirname -- "$directory")"
        mkdir -m 755 -- "$directory"
    fi
    [ -d "$directory" ] || fail 'Host helper parent is not a directory'
    check_tree "$directory"
    check_tree "$(realpath -e -- "$directory")"
done

if [ -e /usr/bin/lianli-control ] || [ -L /usr/bin/lianli-control ]; then
    check_tree /usr/bin/lianli-control
    check_tree "$(realpath -e /usr/bin/lianli-control)"
    [ -f /usr/bin/lianli-control ] && [ -x /usr/bin/lianli-control ] || fail 'The packaged host helper is not executable'
    exec /usr/bin/lianli-control install-container-services --deployment "$deployment"
fi

destination=/usr/local/libexec/lianli/lianli-control
if [ -e "$destination" ] || [ -L "$destination" ]; then
    [ -f "$destination" ] && [ ! -L "$destination" ] || fail 'The standalone helper must be a regular file'
    check_tree "$destination"
fi

stage_path=$(mktemp -d /usr/local/libexec/lianli/.bootstrap-XXXXXXXX)
trap 'rm -rf -- "$stage_path"' EXIT HUP INT TERM
timeout --signal=TERM --kill-after=1s 15s setpriv --reuid="$caller_uid" --regid="$caller_gid" --init-groups --no-new-privs head -c 134217729 -- "$source_binary" > "$stage_path/lianli-control"
[ "$(stat -c %s -- "$stage_path/lianli-control")" -le 134217728 ] || fail 'Host helper exceeds 128 MiB'
actual_digest=$(sha256sum -- "$stage_path/lianli-control")
actual_digest=${actual_digest%% *}
[ "$actual_digest" = "$expected_digest" ] || fail 'The host helper changed during installation'
chmod 755 -- "$stage_path/lianli-control"
sync -f "$stage_path/lianli-control"
mv -T -- "$stage_path/lianli-control" "$destination"
sync -f /usr/local/libexec/lianli
rm -rf -- "$stage_path"
trap - EXIT HUP INT TERM
exec "$destination" install-container-services --deployment "$deployment"
