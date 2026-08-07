#!/usr/bin/env bash
# Emits deterministic SHA256SUMS for every top-level release artifact.
#
# Usage: tools/release/checksums.sh <artifact-dir> [sha256sum-binary]
set -euo pipefail

artifact_dir="${1:?usage: checksums.sh <artifact-dir> [sha256sum-binary]}"
sum_bin="${2:-}"

if [[ ! -d "$artifact_dir" ]]; then
    echo "checksums.sh: $artifact_dir is not a directory" >&2
    exit 1
fi
if [[ -z "$sum_bin" ]]; then
    if command -v sha256sum >/dev/null 2>&1; then
        sum_bin="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        sum_bin="shasum"
    else
        echo "checksums.sh: neither sha256sum nor shasum is available" >&2
        exit 1
    fi
fi

output="$artifact_dir/SHA256SUMS"
temporary="$(mktemp "$artifact_dir/.SHA256SUMS.XXXXXX")"
trap 'rm -f "$temporary"' EXIT
while IFS= read -r file; do
    if [[ "$sum_bin" == "shasum" ]]; then
        (cd "$artifact_dir" && shasum -a 256 "./$file") >> "$temporary"
    else
        (cd "$artifact_dir" && "$sum_bin" "./$file") >> "$temporary"
    fi
done < <(find "$artifact_dir" -maxdepth 1 -type f ! -name SHA256SUMS ! -name '.SHA256SUMS.*' -exec basename {} \; | LC_ALL=C sort)

if [[ ! -s "$temporary" ]]; then
    echo "checksums.sh: no artifacts found in $artifact_dir" >&2
    exit 1
fi
mv "$temporary" "$output"
trap - EXIT
echo "wrote $output"
