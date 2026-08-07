#!/usr/bin/env bash
# Builds koe-desktop into a hardened-runtime macOS .app bundle.
#
# Environment:
#   KOE_RELEASE_VERSION   semver, with an optional leading "v"
#   KOE_RELEASE_TARGET    Rust target triple (defaults to CARGO_BUILD_TARGET)
#   KOE_SIGNING_IDENTITY  Developer ID Application identity; when omitted an
#                         ad-hoc signature is used for local smoke tests
#   KOE_REQUIRE_SIGNING   set to 1 to reject an omitted signing identity
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
release_version="${KOE_RELEASE_VERSION:-0.0.0}"
version="${release_version#v}"
target="${KOE_RELEASE_TARGET:-${CARGO_BUILD_TARGET:-}}"
signing_identity="${KOE_SIGNING_IDENTITY:-}"

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "build-app-bundle.sh: invalid release version: $release_version" >&2
    exit 2
fi
if [[ "${KOE_REQUIRE_SIGNING:-0}" == 1 && -z "$signing_identity" ]]; then
    echo "build-app-bundle.sh: KOE_SIGNING_IDENTITY is required" >&2
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

bundle="$repo_root/dist/koe.app"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS"
cp "$release_dir/koe-desktop" "$bundle/Contents/MacOS/koe-desktop"
cp packaging/macos/Info.plist "$bundle/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$version" "$bundle/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$bundle/Contents/Info.plist"


identity="${signing_identity:--}"
timestamp_args=()
if [[ -n "$signing_identity" ]]; then
    timestamp_args+=(--timestamp)
fi
codesign --force "${timestamp_args[@]}" --options runtime \
    --entitlements packaging/macos/Entitlements.plist \
    --sign "$identity" "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"

echo "built $bundle"
