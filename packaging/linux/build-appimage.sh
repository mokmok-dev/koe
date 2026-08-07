#!/usr/bin/env bash
# Packages koe-desktop as an AppImage with a caller-provided linuxdeploy.
#
# Production builds intentionally do not download an unpinned executable.
# Fetch and verify linuxdeploy in the release environment, then set:
#   KOE_APPIMAGE_TOOL  verified linuxdeploy executable (required)
#   KOE_RELEASE_TARGET optional Rust target triple
#   KOE_RELEASE_VERSION optional release version used in the output name
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
arch="${ARCH:-$(uname -m)}"
target="${KOE_RELEASE_TARGET:-${CARGO_BUILD_TARGET:-}}"
version="${KOE_RELEASE_VERSION:-0.0.0}"
linuxdeploy="${KOE_APPIMAGE_TOOL:-}"

if [[ -z "$linuxdeploy" || ! -x "$linuxdeploy" ]]; then
    echo "build-appimage.sh: KOE_APPIMAGE_TOOL must name a verified linuxdeploy executable" >&2
    exit 2
fi

cd "$repo_root"
build_args=(--release --locked -p koe-desktop)
release_dir="target/release"
if [[ -n "$target" ]]; then
    build_args+=(--target "$target")
    release_dir="target/$target/release"
fi
cargo build "${build_args[@]}"

appdir="target/appimage/AppDir"
rm -rf "target/appimage"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/metainfo" \
    "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/scalable/apps"
cp "$release_dir/koe-desktop" "$appdir/usr/bin/koe-desktop"
cp packaging/linux/org.mokmok.koe.desktop "$appdir/usr/share/applications/"
cp packaging/linux/org.mokmok.koe.appdata.xml "$appdir/usr/share/metainfo/"
cp packaging/linux/icons/org.mokmok.koe.svg \
    "$appdir/usr/share/icons/hicolor/scalable/apps/"

mkdir -p dist
export APPIMAGE_EXTRACT_AND_RUN=1
export OUTPUT="dist/koe-${version}-${arch}.AppImage"
"$linuxdeploy" --appdir "$appdir" \
    --executable "$appdir/usr/bin/koe-desktop" \
    --desktop-file "$appdir/usr/share/applications/org.mokmok.koe.desktop" \
    --icon-file "$appdir/usr/share/icons/hicolor/scalable/apps/org.mokmok.koe.svg" \
    --output appimage

echo "wrote $OUTPUT"
