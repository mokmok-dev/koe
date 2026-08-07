#!/usr/bin/env python3
"""Regression tests for deterministic notice identities and SPDX choices."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).with_name("notices.py")
SPEC = importlib.util.spec_from_file_location("koe_release_notices", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
notices = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(notices)


class NoticeTests(unittest.TestCase):
    def test_local_source_identity_is_checkout_independent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "vendor" / "crate" / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text("[package]\nname='crate'\nversion='1.0.0'\n")
            package = {
                "name": "crate",
                "version": "1.0.0",
                "manifest_path": str(manifest),
                "source": None,
            }
            self.assertEqual(notices.source_identity(package, root), "path+vendor/crate")
            self.assertNotIn(temporary, notices.package_identity(package, root))

    def test_spdx_and_or_with_expressions_preserve_obligations(self) -> None:
        alternatives = notices.spdx_alternatives(
            "(MIT OR Apache-2.0 WITH LLVM-exception) AND BSD-3-Clause"
        )
        self.assertEqual(
            alternatives,
            [
                frozenset({"MIT", "BSD-3-Clause"}),
                frozenset({"Apache-2.0", "LLVM-exception", "BSD-3-Clause"}),
            ],
        )

    def test_legacy_cargo_slash_is_treated_as_or(self) -> None:
        self.assertEqual(
            notices.spdx_alternatives("MIT/Apache-2.0"),
            [frozenset({"MIT"}), frozenset({"Apache-2.0"})],
        )

    def test_malformed_spdx_expression_is_rejected(self) -> None:
        for expression in ("MIT AND", "OR MIT", "MIT WITH", "(MIT OR Apache-2.0"):
            with self.subTest(expression=expression):
                with self.assertRaises(ValueError):
                    notices.spdx_alternatives(expression)


if __name__ == "__main__":
    unittest.main()
