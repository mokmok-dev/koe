#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo mutants \
  --package koe-capture \
  --no-default-features \
  --jobs "${CARGO_MUTANTS_JOBS:-8}"

status=0
for file in mutants.out/missed.txt mutants.out/timeout.txt; do
    if [[ -s "$file" ]]; then
        echo "uncovered mutants in $file" >&2
        cat "$file" >&2
        status=1
    fi
done

if [[ "$status" -ne 0 ]]; then
    echo "mutation gate failed: every viable mutant must be killed" >&2
fi
exit "$status"
