#!/usr/bin/env bash
# macOS HIL gate for the exact notarized app archive and signed CLI artifact.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin="${KOE_HIL_BIN:?KOE_HIL_BIN must name the downloaded release CLI}"
archive="${KOE_HIL_PACKAGE:?KOE_HIL_PACKAGE must name the downloaded app archive}"
metadata="${KOE_HIL_UPDATE_METADATA:?KOE_HIL_UPDATE_METADATA is required}"
expired_metadata="${KOE_HIL_EXPIRED_UPDATE_METADATA:?KOE_HIL_EXPIRED_UPDATE_METADATA is required}"
tamper_metadata="${KOE_HIL_TAMPER_UPDATE_METADATA:?KOE_HIL_TAMPER_UPDATE_METADATA is required}"
system_wav="${KOE_HIL_SYSTEM_TEST_WAV:-${KOE_HIL_TEST_WAV:-}}"
mic_wav="${KOE_HIL_MIC_TEST_WAV:-}"
mic_player="${KOE_HIL_MIC_PLAYER:-}"
offline_runner="${KOE_HIL_OFFLINE_RUNNER:-}"
model_selector="${KOE_HIL_MODEL_SELECTOR:-}"
platform_gates="${KOE_HIL_PLATFORM_GATES:-}"
root="$(mktemp -d)/hil-root"
duration_secs="${KOE_HIL_DURATION_SECS:-3600}"
(( duration_secs >= 3600 )) || { echo "hil-macos: release soak must run for at least 3600 seconds" >&2; exit 2; }
mkdir -p "$root" "$repo_root/target/hil"

for file in "$bin" "$archive" "$metadata" "$expired_metadata" "$tamper_metadata" "$system_wav" "$mic_wav"; do
    [[ -f "$file" ]] || { echo "hil-macos: missing required artifact/fixture: $file" >&2; exit 2; }
done
[[ -x "$mic_player" ]] || { echo "hil-macos: KOE_HIL_MIC_PLAYER is required" >&2; exit 2; }
[[ -x "$offline_runner" && -n "$model_selector" ]] || { echo "hil-macos: offline runner and model selector are required" >&2; exit 2; }
[[ -x "$platform_gates" ]] || { echo "hil-macos: KOE_HIL_PLATFORM_GATES is required" >&2; exit 2; }
chmod +x "$bin"

install_dir="$(mktemp -d)"
tar -xzf "$archive" -C "$install_dir"
app="$install_dir/koe.app"
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"
xcrun stapler validate "$app"
open "$app"
sleep 3
pkill -f "$app/Contents/MacOS/koe-desktop" 2>/dev/null || true
"$platform_gates" "$bin" "$app" "$root"
rm -rf "$install_dir" # mandatory uninstall of the isolated app copy

codesign --verify --strict --verbose=2 "$bin"
spctl --assess --type execute --verbose=2 "$bin"
"$bin" update --data-root "$root" verify --metadata "$metadata" \
    --target "$archive" --target-name "$(basename "$archive")"
"$bin" --output-format json update --data-root "$root" apply \
    --metadata "$metadata" --target "$bin" --consent > "$root/update.json"
"$bin" --output-format json update --data-root "$root" launch -- capabilities \
    > "$root/launched-capabilities.json"
"$bin" --output-format json update --data-root "$root" rollback > "$root/rollback.json"
"$bin" --output-format json update --data-root "$root" launch -- capabilities \
    > "$root/rollback-capabilities.json"
expect_update_rejection() {
    expected="$1"; shift
    if "$@" >"$root/rejection.out" 2>"$root/rejection.err"; then
        echo "hil-macos: update rejection $expected unexpectedly succeeded" >&2
        exit 1
    fi
    grep -q "$expected" "$root/rejection.err"
}
expect_update_rejection KOE-UPDATE-REPLAY \
    "$bin" update --data-root "$root" apply --metadata "$metadata" --target "$bin" --consent
expect_update_rejection KOE-UPDATE-EXPIRED \
    "$bin" update --data-root "$root" apply --metadata "$expired_metadata" --target "$bin" --consent
tampered="$root/tampered-update"
cp "$bin" "$tampered"
printf x >> "$tampered"
expect_update_rejection KOE-UPDATE-TARGET-DIGEST-MISMATCH \
    "$bin" update --data-root "$root" apply --metadata "$tamper_metadata" --target "$tampered" --consent
"$bin" --output-format json doctor --data-root "$root" > "$root/doctor.json"
"$bin" --output-format json models --data-root "$root" install "$model_selector" \
    --network > "$root/model-install.json"
model_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$root/model-install.json")"
test -n "$model_id"

first_device() {
    "$bin" --output-format json devices list --source "$1" |
        python3 -c 'import json,sys; values=json.load(sys.stdin); print(values[0]["id"] if values else "")'
}
mic_id="$(first_device mic)"
system_id="$(first_device system)"
[[ -n "$mic_id" && -n "$system_id" ]] || { echo "hil-macos: mic and system devices required" >&2; exit 1; }

"$offline_runner" "$bin" record --mic "$mic_id" --system "$system_id" --model "$model_selector" \
    --output "$root" --consent --duration-seconds "$duration_secs" \
    --sample-rate 48000 --channels 1 >"$root/record.out" 2>"$root/record.err" &
record_pid=$!
ready=""
for _ in $(seq 1 120); do
    ready="$(find "$root/sessions" -mindepth 2 -maxdepth 2 -name session.json -print -quit 2>/dev/null || true)"
    if [[ -n "$ready" ]] && python3 -c 'import json,sys; raise SystemExit(json.load(open(sys.argv[1]))["state"] != "recording")' "$ready"; then
        break
    fi
    sleep 0.25
done
[[ -n "$ready" ]] || { echo "hil-macos: capture did not become ready" >&2; kill "$record_pid"; exit 1; }
"$mic_player" "$mic_wav" &
mic_pid=$!
afplay "$system_wav" &
system_pid=$!
trap 'kill "$record_pid" "$mic_pid" "$system_pid" 2>/dev/null || true' EXIT
wait "$record_pid"
wait "$mic_pid" "$system_pid" 2>/dev/null || true
trap - EXIT

session_dir="$(find "$root/sessions" -mindepth 1 -maxdepth 1 -type d | head -1)"
python3 "$repo_root/scripts/hil/report_metrics.py" "$session_dir" \
    "$repo_root/target/hil/metrics.json" \
    --system-expected "$system_wav" --mic-expected "$mic_wav"
test -s "$session_dir/transcript/final.json"
"$bin" models --data-root "$root" remove "$model_id"
