"""Focused tests for the dependency-free OKF bundle validator."""

from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import unittest

REPO = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "check_okf.py"

CONCEPT = """\
---
type: Architecture
title: Widget
description: One sentence about the widget.
---

# Widget
"""

INDEX = """\
---
okf_version: "0.2"
---

# Bundle

* [Widget](widget.md) - One sentence about the widget.
"""

LOG = """\
# Update Log

## 2026-07-31

* **Creation**: Added [Widget](widget.md).
"""


def run(bundle: pathlib.Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), str(bundle)],
        capture_output=True,
        text=True,
        check=False,
    )


class CheckOkfTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.bundle = pathlib.Path(self.temporary_directory.name) / "knowledge"
        self.bundle.mkdir()
        (self.bundle / "index.md").write_text(INDEX, encoding="utf-8")
        (self.bundle / "log.md").write_text(LOG, encoding="utf-8")
        (self.bundle / "widget.md").write_text(CONCEPT, encoding="utf-8")

    def test_conformant_bundle_is_accepted(self) -> None:
        result = run(self.bundle)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("OKF v0.2 conformant", result.stdout)

    def test_missing_type_is_rejected(self) -> None:
        (self.bundle / "widget.md").write_text(
            CONCEPT.replace("type: Architecture\n", ""), encoding="utf-8"
        )
        result = run(self.bundle)
        self.assertEqual(result.returncode, 1)
        self.assertIn("non-empty 'type'", result.stderr)

    def test_unlisted_concept_is_rejected(self) -> None:
        (self.bundle / "orphan.md").write_text(CONCEPT, encoding="utf-8")
        result = run(self.bundle)
        self.assertEqual(result.returncode, 1)
        self.assertIn("orphan.md: not listed in index.md", result.stderr)

    def test_dangling_link_is_rejected(self) -> None:
        (self.bundle / "widget.md").write_text(
            CONCEPT + "\nSee [missing](missing.md).\n", encoding="utf-8"
        )
        result = run(self.bundle)
        self.assertEqual(result.returncode, 1)
        self.assertIn("link target does not exist: missing.md", result.stderr)

    def test_repository_bundle_is_conformant(self) -> None:
        result = run(REPO / "knowledge")
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
