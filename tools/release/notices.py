#!/usr/bin/env python3
"""Generate distributable third-party notices from the locked Cargo graph."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path


def cargo_metadata(lock: Path) -> tuple[list[dict], set[str], Path]:
    root = lock.resolve().parent
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
            str(root / "Cargo.toml"),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    metadata = json.loads(result.stdout)
    return metadata["packages"], set(metadata["workspace_members"]), root


def is_first_party(package: dict, workspace: set[str], root: Path) -> bool:
    manifest = Path(package["manifest_path"]).resolve()
    try:
        relative = manifest.relative_to(root)
    except ValueError:
        return False
    return package["id"] in workspace and relative.parts[:1] != ("vendor",)


def source_identity(package: dict, root: Path) -> str:
    source = package.get("source")
    if source:
        return str(source)
    package_root = Path(package["manifest_path"]).resolve().parent
    try:
        return "path+" + package_root.relative_to(root.resolve()).as_posix()
    except ValueError:
        return "path+external/" + package_root.name


def package_identity(package: dict, root: Path) -> str:
    return f"{package['name']}@{package['version']} ({source_identity(package, root)})"


def spdx_alternatives(expression: str) -> list[frozenset[str]]:
    """Return DNF alternatives; every identifier in one set is required."""
    # Cargo metadata still contains legacy `MIT/Apache-2.0` expressions from
    # older crates; SPDX 2.0 deprecated `/` in favor of `OR`.
    expression = expression.replace("/", " OR ")
    tokens = re.findall(r"[A-Za-z0-9.+-]+|[()]", expression)
    position = 0

    def primary() -> list[frozenset[str]]:
        nonlocal position
        if position >= len(tokens):
            raise ValueError("missing SPDX term")
        if tokens[position] == "(":
            position += 1
            result = disjunction()
            if position >= len(tokens) or tokens[position] != ")":
                raise ValueError("unclosed SPDX expression")
            position += 1
            return result
        identifier = tokens[position]
        if identifier in {"AND", "OR", "WITH"}:
            raise ValueError("invalid SPDX operator position")
        position += 1
        required = {identifier}
        if position < len(tokens) and tokens[position] == "WITH":
            position += 1
            if position >= len(tokens) or tokens[position] in {"AND", "OR", "WITH", "(", ")"}:
                raise ValueError("missing SPDX exception")
            required.add(tokens[position])
            position += 1
        return [frozenset(required)]

    def conjunction() -> list[frozenset[str]]:
        nonlocal position
        result = primary()
        while position < len(tokens) and tokens[position] == "AND":
            position += 1
            right = primary()
            result = [left | other for left in result for other in right]
        return result

    def disjunction() -> list[frozenset[str]]:
        nonlocal position
        result = conjunction()
        while position < len(tokens) and tokens[position] == "OR":
            position += 1
            result.extend(conjunction())
        return result

    if not expression:
        return []
    result = disjunction()
    if position != len(tokens):
        raise ValueError("unsupported SPDX syntax")
    return result


def notice_files(package: dict) -> list[Path]:
    package_root = Path(package["manifest_path"]).resolve().parent
    candidates: set[Path] = set()
    license_file = package.get("license_file")
    if license_file:
        path = Path(license_file)
        candidates.add(path if path.is_absolute() else package_root / path)
    for pattern in ("LICENSE*", "LICENCE*", "COPYING*", "NOTICE*", "COPYRIGHT*"):
        candidates.update(path for path in package_root.glob(pattern) if path.is_file())
    return sorted(candidates, key=lambda path: path.name.casefold())


def read_notice(path: Path) -> str | None:
    try:
        content = path.read_text(encoding="utf-8").strip()
    except (OSError, UnicodeError):
        return None
    return content or None


def markdown(value: str) -> str:
    return value.replace("|", "\\|").replace("\n", " ")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", default="Cargo.lock", type=Path)
    parser.add_argument("--out", default="THIRD_PARTY_NOTICES.md", type=Path)
    args = parser.parse_args()
    if not args.lock.is_file():
        print(f"error: {args.lock} not found", file=sys.stderr)
        return 1
    try:
        packages, workspace, root = cargo_metadata(args.lock)
    except (subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"error: cargo metadata failed: {error}", file=sys.stderr)
        return 1

    dependencies = [
        package for package in packages if not is_first_party(package, workspace, root)
    ]
    dependencies.sort(
        key=lambda package: (
            package["name"],
            package["version"],
            source_identity(package, root),
            package_identity(package, root),
        )
    )
    # Cargo packages occasionally omit a license file from the crate archive.
    # Build a deterministic pool of complete standard texts from other locked
    # packages with the same SPDX identifier, then attribute the package and
    # selected license explicitly rather than publishing only an SPDX label.
    license_pool: dict[str, list[tuple[int, str]]] = {}
    for package in dependencies:
        expression = str(package.get("license") or "")
        try:
            alternatives = spdx_alternatives(expression)
        except ValueError:
            alternatives = []
        identifiers = set().union(*alternatives) if alternatives else set()
        single_identifier = len(identifiers) == 1
        for path in notice_files(package):
            content = read_notice(path)
            if content is None:
                continue
            normalized_name = re.sub(r"[^a-z0-9]", "", path.name.casefold())
            for identifier in identifiers:
                normalized_identifier = re.sub(r"[^a-z0-9]", "", identifier.casefold())
                if normalized_identifier in normalized_name or single_identifier:
                    preference = len(content)
                    if normalized_identifier in normalized_name:
                        preference -= 1_000_000
                    license_pool.setdefault(identifier, []).append((preference, content))
    pooled_text = {
        identifier: min(values, key=lambda item: item[0])[1]
        for identifier, values in license_pool.items()
    }

    unresolved: list[str] = []
    records: list[tuple[dict, list[tuple[str, str]]]] = []
    seen_ids: set[str] = set()
    for package in dependencies:
        package_id = package_identity(package, root)
        if package_id in seen_ids:
            continue
        seen_ids.add(package_id)
        texts: list[tuple[str, str]] = []
        seen_text: set[str] = set()
        for path in notice_files(package):
            content = read_notice(path)
            if content is None:
                continue
            digest = hashlib.sha256(content.encode()).hexdigest()
            if digest not in seen_text:
                texts.append((path.name, content))
                seen_text.add(digest)
        expression = str(package.get("license") or "")
        if not texts:
            try:
                alternatives = spdx_alternatives(expression)
            except ValueError:
                alternatives = []
            selected = next(
                (
                    alternative
                    for alternative in alternatives
                    if all(identifier in pooled_text for identifier in alternative)
                ),
                None,
            )
            if selected is not None:
                authors = ", ".join(str(author) for author in package.get("authors", [])) or "not declared"
                for identifier in sorted(selected):
                    texts.append(
                        (
                            f"Resolved {identifier} license text",
                            f"Package attribution/authors: {authors}\n\n{pooled_text[identifier]}",
                        )
                    )
        if not expression or not texts:
            unresolved.append(package_id)
        records.append((package, texts))

    lines = [
        "# Third-party notices",
        "",
        "Generated from `cargo metadata --locked`. Package identity includes",
        "the resolved source, so same-name/version packages are not collapsed.",
        "Applicable license, copyright, and NOTICE text follows the inventory.",
        "",
        "| Package | Version | Source | License expression |",
        "| --- | --- | --- | --- |",
    ]
    for package, _texts in records:
        lines.append(
            "| {} | {} | `{}` | {} |".format(
                markdown(str(package["name"])),
                markdown(str(package["version"])),
                markdown(source_identity(package, root)),
                markdown(str(package.get("license") or "UNRESOLVED")),
            )
        )
    for package, texts in records:
        lines.extend(
            [
                "",
                f"## {package['name']} {package['version']}",
                "",
                f"- Package ID: `{package_identity(package, root)}`",
                f"- Source: `{source_identity(package, root)}`",
                f"- License expression: `{package.get('license') or 'UNRESOLVED'}`",
            ]
        )
        for filename, content in texts:
            fence = "````" if "```" in content else "```"
            lines.extend(["", f"### {filename}", "", f"{fence}text", content, fence])
    if unresolved:
        lines.extend(
            [
                "",
                "## Entries requiring release review",
                "",
                "These resolved package IDs lack either a license expression or",
                "readable distributable license/notice text:",
                "",
                *(f"- `{package_id}`" for package_id in unresolved),
                "",
                "<!-- RELEASE GATE: resolve every entry above before publishing -->",
            ]
        )

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {args.out} ({len(records)} package IDs, {len(unresolved)} unresolved)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
