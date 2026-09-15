#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl git devscripts \
    equivs rsync lintian binutils build-essential
stage=$(mktemp -d "${TMPDIR:-/tmp}/lianli-build-deps.XXXXXX")
cd "$stage"
# equivs writes its archive into TMPDIR; mk-build-deps installs from the working directory.
TMPDIR="$stage" mk-build-deps --arch "$(dpkg --print-architecture)" --install --remove \
    --tool 'apt-get -y --no-install-recommends' "$root/debian/control"
