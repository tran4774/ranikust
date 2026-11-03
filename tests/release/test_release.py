#!/usr/bin/env python3
"""Tests of scripts/release.py (ADR-0089). Run: `just test-release`, or
`python3 -I tests/release/test_release.py` from the repository root.

They need python3 3.11 or later and git, no network: `gh` and the remote
are replaced by fakes.
"""

from __future__ import annotations

import importlib.util
import io
import os
import stat
import sys
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("release", ROOT / "scripts" / "release.py")
rel = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(rel)


def quiet(fn, *args):
    """Run fn, which must die() (SystemExit); fail the test if it returns."""
    with redirect_stderr(io.StringIO()):
        with ExpectDie() as outcome:
            fn(*args)
    return outcome


class ExpectDie:
    def __enter__(self):
        return self

    def __exit__(self, kind, value, tb):
        if kind is None:
            raise AssertionError("expected die(), it returned")
        if issubclass(kind, SystemExit):
            self.code = value.code
            return True
        return False


class Versions(unittest.TestCase):
    def test_semver_precedence(self):
        newer = [
            ("0.0.1", "0.0.0"),
            ("0.1.0", "0.0.9"),
            ("1.0.0", "0.99.99"),
            ("0.1.0", "0.1.0-rc.1"),
            ("0.1.0-rc.2", "0.1.0-rc.1"),
            ("0.1.0-rc.10", "0.1.0-rc.2"),
            ("0.1.0-rc.1", "0.1.0-alpha"),
            ("0.1.0-alpha.1", "0.1.0-alpha"),
        ]
        for new, old in newer:
            self.assertTrue(rel.is_newer(new, old), f"{new} > {old}")
            self.assertFalse(rel.is_newer(old, new), f"{old} < {new}")
        self.assertFalse(rel.is_newer("0.0.1", "0.0.1"))

    def test_check_version_refuses_a_v_and_junk(self):
        for bad in ["v0.0.1", "0.0", "01.0.0", "0.0.1 ", "0.0.1;x", "١.٠.٠"]:
            quiet(rel.check_version, bad)
        rel.check_version("0.0.1")
        rel.check_version("0.1.0-rc.1")


class SourceDateEpoch(unittest.TestCase):
    def test_only_ascii_digits(self):
        for bad in ["١٢٣", "１２", "-1", "1.5", " 12"]:
            with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": bad}):
                quiet(rel.source_date_epoch)
        with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "1700000000"}):
            self.assertEqual(rel.source_date_epoch(), 1700000000)


class CargoToml(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.toml = Path(self.dir.name) / "Cargo.toml"
        self.patch = mock.patch.object(rel, "CARGO_TOML", self.toml)
        self.patch.start()

    def tearDown(self):
        self.patch.stop()
        self.dir.cleanup()

    def test_missing_repository_dies(self):
        self.toml.write_text('[workspace.package]\nversion = "0.0.0"\n')
        quiet(rel.repository)

    def test_missing_version_dies(self):
        self.toml.write_text("[workspace.package]\nedition = \"2024\"\n")
        quiet(rel.workspace_version)

    def test_set_version_touches_only_the_workspace_table(self):
        text = (
            '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.0.0"\n'
            'edition = "2024"\n\n[workspace.dependencies]\nfoo = { version = "1.0" }\n'
        )
        out = rel.set_workspace_version(text, "0.0.1")
        self.assertIn('[workspace.package]\nversion = "0.0.1"\n', out)
        self.assertIn('foo = { version = "1.0" }', out)


class Changelog(unittest.TestCase):
    TEXT = (
        "# Changelog\n\n## [Unreleased]\n\n### Security\n\n- old entry\n\n"
        "[Unreleased]: https://github.com/o/r/commits/main\n"
    )

    def test_pr_changelog_sections(self):
        self.assertIsNone(rel.changelog_text("## Summary\nx\n"))
        self.assertIsNone(rel.changelog_text("## Changelog\n\nNone.\n## Test plan\n"))
        self.assertIsNone(rel.changelog_text("## Changelog\n<!-- only a comment -->\n"))
        self.assertIsNone(rel.changelog_text(None))
        self.assertEqual(
            rel.changelog_text("## Changelog\r\n- a\r\n\r\n## Test plan\r\nx"), "- a"
        )

    def test_draft_groups_and_links(self):
        prs = [
            {"number": 3, "title": "feat(x): y", "url": "u3", "labels": [],
             "body": "## Changelog\n- one,\n  wrapped\n- two\n"},
            {"number": 4, "title": "fix: z", "url": "u4", "labels": [{"name": "security"}],
             "body": "## Changelog\nplain text\n"},
            {"number": 5, "title": "refactor: w", "url": "u5", "labels": [],
             "body": "## Changelog\n### Removed\n- gone\n### fixed\n- fixed\n"},
            {"number": 6, "title": "ci: v", "url": "u6", "labels": [], "body": "none"},
        ]
        body, without = rel.draft_section(prs)
        self.assertEqual([p["number"] for p in without], [6])
        self.assertIn("### Added\n\n- one,\n  wrapped ([#3](u3))\n- two ([#3](u3))", body)
        self.assertIn("### Security\n\n- plain text ([#4](u4))", body)
        self.assertIn("### Removed\n\n- gone ([#5](u5))", body)
        self.assertIn("### Fixed\n\n- fixed ([#5](u5))", body)

    def test_insert_and_links(self):
        out = rel.insert_section(self.TEXT, "0.0.1", None, "### Added\n\n- x", "o/r")
        self.assertIn("## [Unreleased]\n\n## [0.0.1] - ", out)
        self.assertIn("[Unreleased]: https://github.com/o/r/compare/v0.0.1...HEAD", out)
        self.assertIn("[0.0.1]: https://github.com/o/r/releases/tag/v0.0.1", out)
        out2 = rel.insert_section(out, "0.0.2", "v0.0.1", "", "o/r")
        self.assertIn("[0.0.2]: https://github.com/o/r/compare/v0.0.1...v0.0.2", out2)

    def test_insert_needs_unreleased(self):
        quiet(rel.insert_section, "# Changelog\n", "0.0.1", None, "", "o/r")

    def test_notes_refuse_missing_empty_and_draft(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "CHANGELOG.md"
            with mock.patch.object(rel, "CHANGELOG", path):
                path.write_text(self.TEXT)
                quiet(rel.cmd_notes, "0.0.1")
                path.write_text("## [0.0.1] - 2026-01-01\n\n<!-- x -->\n\n## [0.0.0]\n")
                quiet(rel.cmd_notes, "0.0.1")
                path.write_text(rel.insert_section(self.TEXT, "0.0.1", None, "- x", "o/r"))
                quiet(rel.cmd_notes, "0.0.1")
                path.write_text("## [0.0.1] - 2026-01-01\n\n- shipped\n\n[0.0.1]: u\n")
                with redirect_stderr(io.StringIO()), mock.patch("sys.stdout", io.StringIO()) as out:
                    rel.cmd_notes("0.0.1")
                self.assertEqual(out.getvalue(), "- shipped\n")


class Environment(unittest.TestCase):
    GOOD_ENV = {
        "protection_rules": [
            {"type": "required_reviewers", "reviewers": [{"type": "User", "reviewer": {"login": "m"}}]},
            {"type": "branch_policy"},
        ],
        "deployment_branch_policy": {"protected_branches": False, "custom_branch_policies": True},
    }
    GOOD_POLICIES = {"branch_policies": [{"name": "v*", "type": "tag"}]}

    def test_protected_environment_passes(self):
        self.assertEqual(rel.environment_problems(self.GOOD_ENV, self.GOOD_POLICIES), [])

    def test_unprotected_environments_fail(self):
        no_reviewer = dict(self.GOOD_ENV, protection_rules=[{"type": "branch_policy"}])
        empty_reviewers = dict(
            self.GOOD_ENV,
            protection_rules=[{"type": "required_reviewers", "reviewers": []}],
        )
        any_ref = dict(self.GOOD_ENV, deployment_branch_policy=None)
        protected_branches = dict(
            self.GOOD_ENV,
            deployment_branch_policy={"protected_branches": True, "custom_branch_policies": False},
        )
        for env in [no_reviewer, empty_reviewers, any_ref, protected_branches]:
            self.assertNotEqual(rel.environment_problems(env, self.GOOD_POLICIES), [], env)
        for policies in [
            {"branch_policies": []},
            {"branch_policies": [{"name": "main", "type": "branch"}]},
            {"branch_policies": [{"name": "v*", "type": "tag"}, {"name": "*", "type": "tag"}]},
        ]:
            self.assertNotEqual(rel.environment_problems(self.GOOD_ENV, policies), [], policies)


class FakeTools(unittest.TestCase):
    """`gh` and `git ls-remote` replaced by scripts on PATH."""

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.bin = Path(self.dir.name)

    def tearDown(self):
        self.dir.cleanup()

    def fake(self, name, script):
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + script)
        path.chmod(path.stat().st_mode | stat.S_IXUSR)

    def path_env(self):
        return mock.patch.dict(os.environ, {"PATH": f"{self.bin}:{os.environ['PATH']}"})

    def test_a_failing_gh_dies(self):
        self.fake("gh", "echo 'HTTP 502' >&2\nexit 1\n")
        with self.path_env():
            quiet(rel.merged_pull_requests, "o/r", None)

    def test_a_missing_environment_dies(self):
        self.fake("gh", "echo 'gh: Not Found (HTTP 404)' >&2\nexit 1\n")
        with self.path_env():
            quiet(rel.cmd_check_environment, "o/r", "release")

    def test_an_unprotected_environment_dies(self):
        self.fake(
            "gh",
            'case "$2" in\n'
            '  */deployment-branch-policies) echo \'{"branch_policies": []}\' ;;\n'
            '  *) echo \'{"protection_rules": [], "deployment_branch_policy": null}\' ;;\n'
            "esac\n",
        )
        with self.path_env():
            quiet(rel.cmd_check_environment, "o/r", "release")


class PreviousTag(unittest.TestCase):
    def test_previous_release_tag_on_the_history(self):
        with tempfile.TemporaryDirectory() as d:
            env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null",
                       GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t",
                       GIT_COMMITTER_EMAIL="t@t")

            def git(*args):
                rel.subprocess.run(["git", "-C", d, *args], check=True, env=env,
                                   capture_output=True)

            git("init", "-q")
            for tag in ["v0.0.1", "not-a-release", "v0.0.2"]:
                git("commit", "-q", "--allow-empty", "-m", tag)
                git("tag", "-a", tag, "-m", tag)
            with mock.patch.object(rel, "ROOT", Path(d)), mock.patch.dict(os.environ, env):
                self.assertEqual(rel.previous_tag("v0.0.2"), "v0.0.1")
                self.assertIsNone(rel.previous_tag("v0.0.1"))


class Prepare(unittest.TestCase):
    """cmd_prepare leaves no half-edited tree and refuses old or taken versions."""

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        d = Path(self.dir.name)
        self.toml = d / "Cargo.toml"
        self.changelog = d / "CHANGELOG.md"
        self.toml.write_text(
            '[workspace.package]\nversion = "0.0.0"\n'
            'repository = "https://github.com/o/r"\n'
        )
        self.patches = [
            mock.patch.object(rel, "ROOT", d),
            mock.patch.object(rel, "CARGO_TOML", self.toml),
            mock.patch.object(rel, "CHANGELOG", self.changelog),
            mock.patch.object(rel, "run", lambda args, check=True: ""),
            mock.patch.object(rel, "last_tag", lambda: None),
            mock.patch.object(rel, "remote_tag_exists", lambda tag: False),
            mock.patch.object(rel, "merged_pull_requests", lambda repo, since: []),
        ]
        for p in self.patches:
            p.start()

    def tearDown(self):
        for p in self.patches:
            p.stop()
        self.dir.cleanup()

    def test_no_unreleased_heading_leaves_cargo_toml_alone(self):
        self.changelog.write_text("# Changelog\n")
        before = self.toml.read_text()
        quiet(rel.cmd_prepare, "0.0.1")
        self.assertEqual(self.toml.read_text(), before)

    def test_older_version_refused(self):
        self.toml.write_text(self.toml.read_text().replace('"0.0.0"', '"0.2.0"'))
        self.changelog.write_text(Changelog.TEXT)
        before = self.toml.read_text()
        quiet(rel.cmd_prepare, "0.1.9")
        self.assertEqual(self.toml.read_text(), before)

    def test_remote_tag_refused(self):
        self.changelog.write_text(Changelog.TEXT)
        with mock.patch.object(rel, "remote_tag_exists", lambda tag: True):
            quiet(rel.cmd_prepare, "0.0.1")
        self.assertIn('"0.0.0"', self.toml.read_text())

    def test_failed_lock_refresh_restores_cargo_toml(self):
        self.changelog.write_text(Changelog.TEXT)
        before = self.toml.read_text()
        failed = mock.Mock(returncode=101, stderr="error: no workspace", stdout="")
        with mock.patch.object(rel.subprocess, "run", return_value=failed):
            quiet(rel.cmd_prepare, "0.0.1")
        self.assertEqual(self.toml.read_text(), before)
        self.assertEqual(self.changelog.read_text(), Changelog.TEXT)


class Package(unittest.TestCase):
    def test_reproducible_and_complete(self):
        with tempfile.TemporaryDirectory() as d:
            binary = Path(d) / "ranikust"
            binary.write_bytes(b"\x7fELF fake")
            outs = []
            with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "1700000000"}):
                for name in ["a", "b"]:
                    with redirect_stderr(io.StringIO()), mock.patch("sys.stdout", io.StringIO()):
                        rel.cmd_package(str(binary), "0.0.1", str(Path(d) / name))
                    outs.append((Path(d) / name / "ranikust-0.0.1-x86_64-unknown-linux-musl.tar.gz").read_bytes())
            self.assertEqual(outs[0], outs[1])
            with tarfile.open(fileobj=io.BytesIO(outs[0])) as tar:
                members = {m.name: m for m in tar.getmembers()}
            stem = "ranikust-0.0.1-x86_64-unknown-linux-musl"
            for name in ["ranikust", "INSTALL", "LICENSE-APACHE", "LICENSE-MIT", "NOTICE", "README.md"]:
                self.assertIn(f"{stem}/{name}", members)
            self.assertEqual(members[f"{stem}/ranikust"].mode, 0o755)
            self.assertEqual(members[f"{stem}/ranikust"].mtime, 1700000000)

    def test_install_note_has_the_strict_verification(self):
        text = rel.INSTALL.format(version="0.0.1", target=rel.TARGET, repo="tran4774/ranikust")
        for flag in [
            "--signer-workflow tran4774/ranikust/.github/workflows/release.yml",
            "--source-ref refs/tags/v0.0.1",
            "--deny-self-hosted-runners",
        ]:
            self.assertIn(flag, text)


if __name__ == "__main__":
    unittest.main(verbosity=1)
