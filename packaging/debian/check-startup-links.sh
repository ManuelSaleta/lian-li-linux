#!/usr/bin/env bash
set -euo pipefail

stage=$(realpath -e "$1")
links=$(mktemp "${TMPDIR:-/tmp}/lianli-startup-links.XXXXXX")
trap 'rm -f "$links"' EXIT
find "$stage" -type l -path '*.wants/*' -print0 > "$links"
while IFS= read -r -d '' link; do
    case "${link#"$stage"/}" in
        usr/lib/systemd/system/multi-user.target.wants/lianli-control-recovery.service)
            expected=../lianli-control-recovery.service ;;
        usr/lib/systemd/user/default.target.wants/lianli-session.service)
            expected=../lianli-session.service ;;
        *)
            echo "Unexpected automatic service startup: ${link#"$stage"/}" >&2
            exit 1 ;;
    esac
    if [ "$(readlink "$link")" != "$expected" ]; then
        echo "Unexpected startup target for ${link#"$stage"/}" >&2
        exit 1
    fi
done < "$links"
