#!/usr/bin/env bash
#
# Reads back what macOS requires before a hardened-runtime signature may open
# the microphone. Without com.apple.security.device.audio-input the runtime
# refuses audio input before TCC is consulted, so `AudioUnitRender` returns
# -10863 and no permission prompt ever appears.
#
# usage: verify-macos-bundle.sh <path to .app>
set -euo pipefail

bundle="${1-}"
if [ -z "$bundle" ]; then
  echo "usage: $0 <path to .app>" >&2
  exit 1
fi
if [ ! -d "$bundle" ]; then
  echo "no app bundle at $bundle" >&2
  exit 1
fi

# `codesign -d --entitlements` writes the plist to stderr on some versions, so
# both streams are captured.
if ! entitlements="$(codesign -d --entitlements :- "$bundle" 2>&1)"; then
  echo "codesign could not read the signature of $bundle" >&2
  printf '%s\n' "$entitlements" >&2
  exit 1
fi

key="com.apple.security.device.audio-input"
if ! grep -Fq "<key>$key</key>" <<<"$entitlements"; then
  echo "missing entitlement $key in $bundle" >&2
  exit 1
fi
# The value may sit on the next line or beside the key, so the check skips the
# whitespace between them.
if ! awk -v key="<key>$key</key>" '
  { text = text $0 " " }
  END {
    rest = substr(text, index(text, key) + length(key))
    sub(/^[[:space:]]+/, "", rest)
    exit (substr(rest, 1, 7) == "<true/>") ? 0 : 1
  }
' <<<"$entitlements"; then
  echo "entitlement $key is not <true/> in $bundle" >&2
  exit 1
fi
echo "ok: $bundle carries $key"

plist="$bundle/Contents/Info.plist"
if [ ! -f "$plist" ]; then
  echo "no Info.plist in $bundle" >&2
  exit 1
fi
if ! bundle_plist="$(plutil -convert xml1 -o - "$plist")"; then
  echo "plutil could not read $plist" >&2
  exit 1
fi
if ! grep -Fq '<key>NSMicrophoneUsageDescription</key>' <<<"$bundle_plist"; then
  echo "missing NSMicrophoneUsageDescription in $plist" >&2
  exit 1
fi
echo "ok: $bundle declares NSMicrophoneUsageDescription"

echo "ok"
