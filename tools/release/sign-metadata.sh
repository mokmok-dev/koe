#!/usr/bin/env bash
# Sign one platform release directory. The signing seed is accepted only via
# KOE_UPDATE_SIGNING_SEED_HEX (normally a protected CI secret), never argv.
set -euo pipefail

app_version=""
platform=""
install_target=""
metadata_version=""
expires_unix_s=""
artifact_dir=""
out=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --app-version) app_version="$2"; shift 2 ;;
        --platform) platform="$2"; shift 2 ;;
        --install-target) install_target="$2"; shift 2 ;;
        --metadata-version) metadata_version="$2"; shift 2 ;;
        --expires-unix-s) expires_unix_s="$2"; shift 2 ;;
        --artifact-dir) artifact_dir="$2"; shift 2 ;;
        --out) out="$2"; shift 2 ;;
        *) echo "sign-metadata.sh: unknown option: $1" >&2; exit 2 ;;
    esac
done
for required in app_version platform install_target metadata_version expires_unix_s artifact_dir; do
    [[ -n "${!required}" ]] || { echo "sign-metadata.sh: --${required//_/-} is required" >&2; exit 2; }
done
[[ -n "${KOE_UPDATE_SIGNING_SEED_HEX:-}" ]] || {
    echo "sign-metadata.sh: KOE_UPDATE_SIGNING_SEED_HEX is required" >&2
    exit 2
}

args=(sign --app-version "$app_version" --platform "$platform"
      --install-target "$install_target" --expires-unix-s "$expires_unix_s"
      --metadata-version "$metadata_version" --artifact-dir "$artifact_dir")
[[ -z "$out" ]] || args+=(--out "$out")
if [[ -x target/release/koe-release-sign ]]; then
    bin=target/release/koe-release-sign
elif [[ -x target/debug/koe-release-sign ]]; then
    bin=target/debug/koe-release-sign
else
    cargo build --release --locked -p koe-update
    bin=target/release/koe-release-sign
fi
"$bin" "${args[@]}"
echo "signed metadata for $platform $app_version" >&2
