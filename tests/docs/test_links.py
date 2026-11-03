#!/usr/bin/env python3
"""Tests the link check of scripts/docs-lint.py on a throwaway tree.

Run: `just test-docs-lint` (python3 -I tests/docs/test_links.py). No network.
"""

import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("docs_lint", ROOT / "scripts" / "docs-lint.py")
docs_lint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(docs_lint)

DESIGN = """# Design

## Overview

## `Direct` (kaniko mode on `/`)

## Overview

<a name="old-name"></a>
## New name
"""


class LinkCheck(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        docs_lint.ROOT = self.root
        docs_lint._anchors.clear()
        self.write("docs/design/rootfs.md", DESIGN)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def problems(self, rel, text):
        self.write(rel, text)
        found = []
        docs_lint.link_checks(found, [rel])
        return found

    def assertClean(self, rel, text):
        self.assertEqual(self.problems(rel, text), [])

    def assertFlags(self, rel, text, needle):
        found = self.problems(rel, text)
        self.assertEqual(len(found), 1, found)
        self.assertIn(needle, found[0])

    def test_links_to_files_and_headings(self):
        self.assertClean("docs/a.md", "[d](design/rootfs.md) [o](design/rootfs.md#overview)\n")
        self.assertClean("docs/a.md", "[d](design/rootfs.md#direct-kaniko-mode-on-)\n")
        self.assertClean("docs/a.md", "[second](design/rootfs.md#overview-1) [alias](design/rootfs.md#old-name)\n")
        self.assertClean("docs/a.md", "[dir](design/) [self](#top)\n\n# Top\n")
        self.assertFlags("docs/a.md", "[x](design/gone.md)\n", "no such file")
        self.assertFlags("docs/a.md", "[x](design/rootfs.md#tests)\n", "has no anchor #tests")
        self.assertFlags("docs/a.md", "[x](#nowhere)\n", "has no anchor #nowhere")

    def test_relative_to_the_linking_file(self):
        # mdBook resolves from docs/book/src/, so ../adr/ there is docs/book/adr/.
        self.write("docs/adr/0001-x.md", "# ADR\n")
        self.assertFlags("docs/book/src/usage.md", "[ADR-0001](../adr/0001-x.md)\n", "no such file")
        self.assertClean("docs/book/src/usage.md", "[ADR-0001](../../adr/0001-x.md)\n")

    def test_code_and_fences_are_not_links(self):
        self.assertClean("docs/a.md", "`[x](gone.md)`\n```\n[x](gone.md)\n```\n")

    def test_reference_definitions(self):
        self.assertFlags("docs/a.md", "[ref]: design/gone.md\n", "no such file")
        self.assertClean("docs/a.md", '[ref]: design/rootfs.md#overview "Overview"\n')
        # An evidence label is prose, not a definition.
        self.assertClean("docs/a.md", "  [code]: opt-in by an environment variable\n")

    def test_links_to_this_repository_on_github(self):
        base = "https://github.com/tran4774/ranikust/blob/main/"
        self.assertClean("docs/a.md", f"[r]({base}docs/design/rootfs.md#overview)\n")
        self.assertFlags("docs/a.md", f"[r]({base}docs/design/gone.md)\n", "no such file")
        self.assertFlags("docs/a.md", f"[r]({base}docs/design/rootfs.md#gone)\n", "no anchor #gone")
        self.assertClean("docs/a.md", "[o](https://example.org/x.md) [m](mailto:someone)\n")

    def test_paths_named_in_code_spans_and_code(self):
        self.assertClean("docs/a.md", "see `docs/design/rootfs.md#overview`\n")
        self.assertFlags("docs/a.md", "see `docs/design/rootfs.md#tests`\n", "has no anchor #tests")
        self.assertFlags("docs/a.md", "see `docs/design/gone.md`\n", "no such file")
        self.assertFlags("crates/x/src/lib.rs", "//! See docs/design/rootfs.md#gone.\n", "has no anchor #gone")
        self.assertClean("crates/x/src/lib.rs", "//! See docs/design/rootfs.md#overview.\n")
        # Test data, bare names and placeholders are not references.
        self.assertClean("crates/x/src/tests.rs", 'let keep = "docs/keep.md";\n')
        self.assertClean("tests/ci/test_ci.py", 'FAKE = "docs/adr/0095-x.md#gone"\n')
        self.assertClean("docs/a.md", "`SKILL.md`, `docs/agents/phase-N.md`, `docs/adr/NNNN-title.md`\n")


if __name__ == "__main__":
    unittest.main(verbosity=1)
