#!/usr/bin/env bash
set -euo pipefail

manifest=$(cargo metadata --locked --offline --filter-platform x86_64-unknown-linux-gnu --format-version=1 | \
    jq -er '.packages[] | select(.name == "turbojpeg-sys") | .manifest_path')
source_dir="${manifest%/*}/libjpeg-turbo"
install -Dm644 "$source_dir/LICENSE.md" "$1/libjpeg-turbo-LICENSE.md"
install -Dm644 "$source_dir/README.ijg" "$1/libjpeg-turbo-README.ijg"
