#!/usr/bin/env python3
"""Tests the link check of scripts/docs-lint.py on throwaway trees.

Run: `just test-docs-lint` (python3 -I tests/docs/test_links.py). No network.
"""

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "docs-lint.py"
spec = importlib.util.spec_from_file_location("docs_lint", SCRIPT)
docs_lint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(docs_lint)
BASE = "https://github.com/tran4774/ranikust/"

DESIGN = """---
# a front-matter comment, not a heading
---

# Design

## Overview

## `Direct` (kaniko mode on `/`)

## Overview

<a name="old-name"></a>
## New name

### Q-051: Does `FROM <stage>` match stage names?

## Overlay ([experiment 06][e06])

## The _real_ snake_case

## A &amp; B

Setext title
------------

<a class="c" id='single'></a>

````
```
## Not a heading, inside a four-backtick fence
```
````

## After the fence

[e06]: ../research/e06.md
"""


class LinkCheck(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        docs_lint.ROOT = self.root
        docs_lint._anchors.clear()
        self.files = []
        self.write("docs/design/rootfs.md", DESIGN)
        self.write("docs/research/e06.md", "# E06\n")
        self.write("README.md", "# ranikust\n\n## Roadmap\n")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        if rel not in self.files:
            self.files.append(rel)

    def problems(self, rel, text):
        self.write(rel, text)
        found = []
        docs_lint.link_checks(found, self.files)
        # Only what this file reports: earlier cases' files stay on the tree.
        return [p for p in found if p.startswith(f"{rel}:")]

    def assertClean(self, rel, text):
        self.assertEqual(self.problems(rel, text), [])

    def assertFlags(self, rel, text, needle):
        found = self.problems(rel, text)
        self.assertEqual(len(found), 1, found)
        self.assertIn(needle, found[0])
        return found[0]

    def test_anchors_follow_github(self):
        for anchor in ("overview", "overview-1", "direct-kaniko-mode-on-", "old-name", "new-name",
                       "q-051-does-from-stage-match-stage-names", "overlay-experiment-06",
                       "the-real-snake_case", "a--b", "setext-title", "single", "after-the-fence"):
            with self.subTest(anchor=anchor):
                self.assertClean("docs/a.md", f"[x](design/rootfs.md#{anchor})\n")
        for anchor in ("not-a-heading-inside-a-four-backtick-fence", "a-front-matter-comment-not-a-heading"):
            with self.subTest(anchor=anchor):
                self.assertFlags("docs/a.md", f"[x](design/rootfs.md#{anchor})\n", f"no anchor #{anchor}")

    def test_links_to_files(self):
        self.assertClean("docs/a.md", "[d](design/rootfs.md) [dir](design/) [self](#top)\n\n# Top\n")
        self.assertClean("docs/a.md", "[root](/docs/design/rootfs.md#overview) [enc](design/root%66s.md)\n")
        self.assertClean("docs/a.md", "[t](design/rootfs.md 'Title') [p](design/rootfs.md (Title))\n")
        self.assertClean("docs/a.md", "[s](<design/rootfs.md>) ![i](design/rootfs.md)\n")
        self.assertFlags("docs/a.md", "[x](design/gone.md)\n", "no such file")
        self.assertFlags("docs/a.md", "![x](design/gone.png)\n", "no such file")
        self.assertFlags("docs/a.md", '<a href="design/gone.md">x</a>\n', "no such file")
        self.assertFlags("docs/a.md", "[x](design/rootfs.md#tests)\n", "has no anchor #tests")
        self.assertFlags("docs/a.md", "[x](#nowhere)\n", "has no anchor #nowhere")
        self.assertFlags("docs/a.md", "[x](../../../etc/passwd)\n", "outside the repository")

    def test_only_listed_files_count(self):
        # On disk but not in git ls-files (target/, an ignored file): absent in CI.
        (self.root / "target").mkdir()
        (self.root / "target" / "x.md").write_text("# X\n")
        self.assertFlags("docs/a.md", "[x](../target/x.md)\n", "no such file")

    def test_the_book_links_inside_its_source(self):
        self.write("docs/adr/0001-x.md", "# ADR\n")
        self.write("docs/book/src/install.md", "# Install\n")
        self.assertFlags("docs/book/src/usage.md", "[A](../adr/0001-x.md)\n", "no such file")
        self.assertFlags("docs/book/src/usage.md", "[A](../../adr/0001-x.md)\n",
                         "mdBook publishes docs/book/src only")
        self.assertClean("docs/book/src/usage.md",
                         f"[i](install.md#install) [A]({BASE}blob/main/docs/adr/0001-x.md)\n")
        self.assertClean("docs/book/src/status.md", f"[r]({BASE}#roadmap) [home]({BASE}#readme)\n")

    def test_code_spans_and_fences_are_not_links(self):
        self.assertClean("docs/a.md", "`[x](gone.md)`\n```\n[x](gone.md)\n```\n~~~\n```\n[x](gone.md)\n~~~\n")
        # A span wrapped onto the next line still shields what it holds.
        self.assertFlags("docs/a.md", "a `span that\nwraps [x](gone.md)` then [y](design/gone.md)\n",
                         "design/gone.md")

    def test_reference_definitions_and_uses(self):
        self.assertFlags("docs/a.md", "[ref]: design/gone.md\n", "no such file")
        self.assertClean("docs/a.md", '[x][ref]\n\n[ref]: design/rootfs.md#overview "Overview"\n')
        self.assertFlags("docs/a.md", "see [the design][design]\n", "no definition of [design]")
        # Evidence labels and indexing are prose, not references.
        self.assertClean("docs/a.md", "  [code]: opt-in by an environment variable\n"
                         'read ["phases"]["rnk_registry.pull"] and [inferred] [code]\n')

    def test_links_to_this_repository_on_github(self):
        self.assertClean("docs/a.md", f"[r]({BASE}blob/main/docs/design/rootfs.md#overview) "
                         f"[t]({BASE}tree/main/docs/design)\n")
        self.assertClean("docs/a.md", f"[l]({BASE}blob/main/README.md#L3)\n")
        self.assertFlags("docs/a.md", f"[r]({BASE}blob/main/docs/design/gone.md)\n", "no such file")
        self.assertFlags("docs/a.md", f"[r]({BASE}blob/main/docs/design/rootfs.md#gone)\n", "no anchor #gone")
        self.assertFlags("docs/a.md", f"[r]({BASE}#gone)\n", "README.md has no anchor #gone")
        self.assertClean("docs/a.md", "[o](https://example.org/x.md) [m](mailto:someone)\n")

    def test_paths_named_in_code_spans_and_code(self):
        self.assertClean("docs/a.md", "see `docs/design/rootfs.md#overview`\n")
        self.assertFlags("docs/a.md", "see `docs/design/rootfs.md#tests`\n", "has no anchor #tests")
        self.assertFlags("docs/a.md", "see `docs/design/gone.md`\n", "no such file")
        self.assertFlags("crates/x/src/lib.rs", "//! See docs/design/rootfs.md#gone.\n", "has no anchor #gone")
        self.assertFlags("tests/vm/vm.sh", "# see docs/testing.md#rules\n", "no such file")
        self.assertClean("crates/x/src/lib.rs", "//! See docs/design/rootfs.md#overview.\n")
        # Examples, test data, bare names and placeholders are not references.
        self.assertClean("crates/x/src/lib.rs", "//! `docs/keep.md` is kept\n")
        self.assertClean("crates/x/src/tests.rs", 'let x = "docs/design/gone.md";\n')
        self.assertClean("tests/ci/test_ci.py", 'FAKE = "docs/adr/0095-x.md#gone"\n')
        self.assertClean("docs/a.md", "`SKILL.md`, `docs/agents/phase-N.md`, `docs/adr/NNNN-title.md`\n")

    def test_what_is_not_read(self):
        self.assertClean("docs/notes.txt", "[x](gone.md)\n")
        self.assertClean("docs/adr/template.md", "[x](../research/experiments/NN-name.md)\n")

    def test_a_frozen_record_says_where_to_fix(self):
        line = self.assertFlags("docs/adr/0001-x.md", "[x](../design/rootfs.md#gone)\n", "no anchor #gone")
        self.assertIn('frozen: add <a name="gone"></a>', line)


class Main(unittest.TestCase):
    def test_a_broken_link_fails_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "docs" / "adr").mkdir(parents=True)
            (root / "docs" / "adr" / "README.md").write_text("# ADRs\n")
            (root / "docs" / "open-questions.md").write_text("# Open questions\n")
            (root / "docs" / "a.md").write_text("[x](gone.md)\n")
            env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
            subprocess.run(["git", "init", "-q"], cwd=root, check=True, env=env)
            run = subprocess.run([sys.executable, "-I", str(SCRIPT)], cwd=root, env=env,
                                 capture_output=True, text=True)
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("docs/a.md:1: gone.md: no such file", run.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=1)
