#!/usr/bin/env bash
# Generate Swift/C bindings from koe-ffi for koe-native consumption.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="${1:-dev}"
out_dir="${root}/koe-native/generated"

cd "${root}"

if [[ "${profile}" == "release" ]]; then
  cargo build -p koe-ffi --release
  lib_path="${root}/target/release/libkoe_ffi.a"
else
  cargo build -p koe-ffi
  lib_path="${root}/target/debug/libkoe_ffi.a"
fi

cargo run -p koe-ffi --bin uniffi-bindgen -- generate \
  --library "${lib_path}" \
  --language swift \
  --out-dir "${out_dir}" \
  --no-format

echo "Generated bindings in ${out_dir}"
