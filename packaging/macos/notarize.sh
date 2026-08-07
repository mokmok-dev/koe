#!/usr/bin/env bash
# Submit a signed app with App Store Connect API credentials, then staple it.
# Secret key material is read from KOE_NOTARIZE_KEY_BASE64 into an owner-only
# temporary file and is never placed in process arguments.
set -euo pipefail

target="${1:?usage: notarize.sh <signed .app or zip archive>}"
required=(KOE_NOTARIZE_KEY_ID KOE_NOTARIZE_ISSUER_ID KOE_NOTARIZE_KEY_BASE64)
for variable in "${required[@]}"; do
    if [[ -z "${!variable:-}" ]]; then
        if [[ "${KOE_REQUIRE_NOTARIZATION:-0}" == 1 ]]; then
            echo "notarize.sh: $variable is required" >&2
            exit 2
        fi
        echo "notarize.sh: API credentials unavailable; skipping notarization" >&2
        exit 0
    fi
done
[[ -d "$target" || -f "$target" ]] || { echo "notarize.sh: signed input not found" >&2; exit 2; }

work_dir="$(mktemp -d)"
chmod 700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT
archive="$work_dir/koe-dist.zip"
result="$work_dir/submission.json"
key_file="$work_dir/AuthKey_${KOE_NOTARIZE_KEY_ID}.p8"
umask 077
# `/usr/bin/base64` on macOS uses `-D` for decode (`--decode` is GNU-only).
printf '%s' "$KOE_NOTARIZE_KEY_BASE64" | /usr/bin/base64 -D > "$key_file"
if [[ -d "$target" ]]; then
    ditto -c -k --keepParent "$target" "$archive"
else
    archive="$target"
fi

credentials=(--key "$key_file" --key-id "$KOE_NOTARIZE_KEY_ID" --issuer "$KOE_NOTARIZE_ISSUER_ID")
xcrun notarytool submit "$archive" "${credentials[@]}" \
    --wait --output-format json > "$result"
submission_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$result")"
status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "$result")"
if [[ "$status" != "Accepted" ]]; then
    xcrun notarytool log "$submission_id" "${credentials[@]}" || true
    echo "notarize.sh: submission $submission_id ended in $status" >&2
    exit 1
fi

if [[ -d "$target" ]]; then
    xcrun stapler staple "$target"
    xcrun stapler validate "$target"
    spctl --assess --type execute --verbose=2 "$target"
    echo "notarized and stapled $target ($submission_id)"
else
    echo "notarized archive $target ($submission_id)"
fi
