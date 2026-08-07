#!/usr/bin/env bash
# Generate and validate exactly one CycloneDX 1.5 SBOM per workspace package.
set -euo pipefail

install_dir="${HOME}/.koe-release-tools"
out_dir="${KOE_SBOM_DIR:-target/release/sbom}"
version="0.5.9"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --install-dir) install_dir="${2:?--install-dir requires a value}"; shift 2 ;;
        --out-dir) out_dir="${2:?--out-dir requires a value}"; shift 2 ;;
        *) echo "sbom.sh: unknown option: $1" >&2; exit 2 ;;
    esac
done

cyclonedx_bin="$install_dir/bin/cargo-cyclonedx"
if [[ ! -x "$cyclonedx_bin" ]] || ! "$cyclonedx_bin" cyclonedx --version | grep -q "$version"; then
    cargo install cargo-cyclonedx --version "$version" --locked --root "$install_dir"
fi

metadata="$(mktemp)"
trap 'rm -f "$metadata"' EXIT
cargo metadata --locked --format-version 1 > "$metadata"
find apps crates -type f -name '*.cdx.json' -delete
"$cyclonedx_bin" cyclonedx --format json --all --spec-version 1.5
rm -rf "$out_dir"
mkdir -p "$out_dir"

python3 - "$metadata" "$out_dir" <<'PY'
import json
import shutil
import sys
from pathlib import Path

metadata = json.loads(Path(sys.argv[1]).read_text())
out = Path(sys.argv[2])
workspace = set(metadata["workspace_members"])
packages = sorted(
    (package for package in metadata["packages"] if package["id"] in workspace),
    key=lambda package: package["id"],
)
expected = set()
for package in packages:
    manifest_dir = Path(package["manifest_path"]).parent
    candidates = sorted(manifest_dir.glob("*.cdx.json"))
    if len(candidates) != 1:
        raise SystemExit(
            f"sbom.sh: expected one generated SBOM for {package['id']}, found {len(candidates)}"
        )
    document = json.loads(candidates[0].read_text())
    if document.get("bomFormat") != "CycloneDX" or document.get("specVersion") != "1.5":
        raise SystemExit(f"sbom.sh: invalid CycloneDX 1.5 document for {package['id']}")
    component = document.get("metadata", {}).get("component", {})
    if component.get("name") != package["name"] or component.get("version") != package["version"]:
        raise SystemExit(f"sbom.sh: root component mismatch for {package['id']}")
    safe = "".join(character if character.isalnum() or character in "._-" else "_" for character in package["name"])
    filename = f"{safe}-{package['version']}.cdx.json"
    if filename in expected:
        raise SystemExit(f"sbom.sh: collision-safe output still collided: {filename}")
    expected.add(filename)
    shutil.copyfile(candidates[0], out / filename)
    candidates[0].unlink()
actual = {path.name for path in out.glob("*.cdx.json")}
if actual != expected:
    raise SystemExit(f"sbom.sh: inventory mismatch expected={sorted(expected)} actual={sorted(actual)}")
print(f"wrote and validated {len(actual)} CycloneDX SBOMs to {out}")
PY
