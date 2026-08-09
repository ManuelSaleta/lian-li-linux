#!/usr/bin/env bash

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
PKG="lian-li-linux"
EVDI_VERSION="1.15.0"

VERSION=$(git describe --tags --abbrev=0 2>/dev/null | sed 's/^v//' || true)
if [ -z "${VERSION:-}" ]; then
    VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
fi
echo ">> Building SRPM for ${PKG} ${VERSION}"

TOPDIR="$PWD/tmp/rpmbuild"
STAGE="$TOPDIR/${PKG}-${VERSION}"
rm -rf "$TOPDIR"
mkdir -p "$TOPDIR"/{SOURCES,SPECS,BUILD,RPMS,SRPMS}

echo ">> Staging sources (working tree + submodules)"
mkdir -p "$STAGE"
rsync -a \
    --exclude='.git' \
    --exclude='target' \
    --exclude='tmp' \
    --exclude='.cache' \
    --exclude='node_modules' \
    --exclude='dist' \
    --exclude='packaging/archlinux/pkg' \
    --exclude='packaging/archlinux/src' \
    --exclude='*.pkg.tar.zst' \
    ./ "$STAGE/"

echo ">> Vendoring cargo dependencies (offline build)"
( cd "$STAGE" && cargo vendor --quiet vendor-crates )
mkdir -p "$STAGE/.cargo"
cat > "$STAGE/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor-crates"
EOF

echo ">> Building Vue frontend with npm into dist/"
# Tauri's build.rs normally drives this with npm, which isn't in the Fedora mock
# buildroot. We pre-build dist/ here so the mock build never needs npm/nodejs.
# The repo ships no JS lockfile, so use `npm install` (not `npm ci`) to resolve
# a dependency tree from package.json.
( cd "$STAGE/crates/lianli-gui" && npm install --no-audit --no-fund && npm run build )
rm -rf "$STAGE/crates/lianli-gui/node_modules"

echo ">> Creating source tarball"
tar -C "$TOPDIR" -czf "$TOPDIR/SOURCES/${PKG}-${VERSION}.tar.gz" "${PKG}-${VERSION}"

echo ">> Fetching evdi ${EVDI_VERSION} source (Source1)"
# libevdi isn't packaged in Fedora; the spec builds it from this pinned source.
curl -fsSL -o "$TOPDIR/SOURCES/evdi-${EVDI_VERSION}.tar.gz" \
    "https://github.com/DisplayLink/evdi/archive/refs/tags/v${EVDI_VERSION}.tar.gz"

echo ">> Assembling SRPM"
SPEC="$TOPDIR/SPECS/${PKG}.spec"
cp packaging/fedora/lian-li-linux.spec "$SPEC"
# Inject the resolved version so Cargo.toml stays the single source of truth.
sed -i "s/^Version:.*/Version:        ${VERSION}/" "$SPEC"
rpmbuild -bs --define "_topdir $TOPDIR" "$SPEC"

SRPM=$(ls "$TOPDIR"/SRPMS/${PKG}-*.src.rpm | head -n1)
echo ">> Built: $SRPM"
# When OUTDIR is set (e.g. by the COPR .copr/Makefile entrypoint), place the
# finished SRPM there so the caller can pick it up.
if [ -n "${OUTDIR:-}" ]; then
    mkdir -p "$OUTDIR"
    cp -f "$SRPM" "$OUTDIR/"
    echo ">> Copied to $OUTDIR/$(basename "$SRPM")"
fi
