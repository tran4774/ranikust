#!/usr/bin/env python3
"""Tests of scripts/ci.py and of the lanes' wiring in the workflows
(ADR-0095). Run: `just test-ci`, or `python3 -I tests/ci/test_ci.py` from the
repository root; CI's `changes` job runs them first.

They need python3 3.11 or later and git, no network and no `gh`.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci.py"
WORKFLOWS = ROOT / ".github" / "workflows"
SPEC = importlib.util.spec_from_file_location("ci", SCRIPT)
ci = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci)

RUST_T2 = {"rust", "t2"}


def run(args, env=None, stdin=None, cwd=ROOT, github_output=True):
    """scripts/ci.py with only the given CI_* variables set, and, as in a
    workflow, a fresh $GITHUB_OUTPUT whenever CI_EVENT is set (unless
    `github_output` is False or the caller names one)."""
    base = {k: v for k, v in os.environ.items() if not k.startswith(("CI_", "GITHUB_"))}
    env = dict(env or {})
    with tempfile.TemporaryDirectory() as d:
        if github_output and "CI_EVENT" in env and "GITHUB_OUTPUT" not in env:
            env["GITHUB_OUTPUT"] = str(Path(d) / "github-output")
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), *args],
            env=base | env, input=stdin, capture_output=True, text=True, cwd=cwd,
            timeout=60, check=False,
        )


def outputs(stdout):
    return dict(line.split("=", 1) for line in stdout.splitlines() if re.fullmatch(r"[a-z0-9_]+=(true|false)", line))


def lanes(stdout):
    out = outputs(stdout)
    return {k for k in ci.LANES if out[k] == "true"}


class Classify(unittest.TestCase):
    def check(self, paths, want, unknown=()):
        got, missing = ci.classify(paths)
        self.assertEqual(set(got), set(want), paths)
        self.assertEqual(missing, list(unknown), paths)

    def test_docs_only_runs_nothing(self):
        self.check(["README.md", "docs/testing.md", "docs/adr/0095-x.md", "CLAUDE.md",
                    ".claude/skills/sync-docs/SKILL.md", ".github/PULL_REQUEST_TEMPLATE.md",
                    "docs/agents/rules.md", "CHANGELOG.md"], set())

    def test_the_book_only_runs_the_book(self):
        self.check(["docs/book/src/install.md"], {"book"})
        self.check(["docs/book/book.toml", "docs/testing.md"], {"book"})

    def test_rust_code_runs_rust_and_t2(self):
        self.check(["crates/rnk-rootfs/src/lib.rs"], RUST_T2)
        self.check(["crates/rnk-oci/testdata/oci-index.json"], RUST_T2)
        self.check(["rustfmt.toml"], RUST_T2)
        self.check(["justfile"], RUST_T2)
        self.check(["scripts/check-static.sh"], RUST_T2)

    def test_manifests_toolchain_and_cargo_config_also_run_deny(self):
        for p in ["Cargo.toml", "Cargo.lock", "crates/rnk-oci/Cargo.toml", "rust-toolchain.toml",
                  ".cargo/config.toml"]:
            self.check([p], RUST_T2 | {"deny"})
        self.check(["deny.toml"], {"deny"})
        # A Cargo.toml deeper in a crate is the crate's, not a member's.
        self.check(["crates/rnk-oci/fuzz/Cargo.toml"], RUST_T2)

    def test_the_t2_harness_runs_t2_alone(self):
        for p in ["tests/vm/runner.sh", "tests/vm/guest/unit.sh", "tests/container/lane.sh",
                  "tests/smoke/cases/run/Dockerfile", "tests/vm/guest/k8s.sh",
                  "tests/vm/guest/k8s/pin.env"]:
            self.check([p], {"t2"})

    def test_the_apparmor_profile_runs_t2_and_the_book(self):
        self.check(["contrib/apparmor/ranikust"], {"t2", "book"})

    def test_the_workflows_and_this_script_run_everything(self):
        for p in [".github/workflows/ci.yml", ".github/workflows/t2.yml",
                  ".github/workflows/release.yml", "scripts/ci.py", ".gitattributes"]:
            self.check([p], ci.ALL)

    def test_what_other_workflows_or_nobody_reads_runs_nothing(self):
        self.check(["tests/release/test_release.py", "scripts/release.py", "licenses/README.md",
                    "about.toml", "NOTICE", "LICENSE-MIT", "LICENSE-APACHE", "cliff.toml",
                    "scripts/docs-lint.py", "tests/docs/test_links.py",
                    "scripts/review-pack.py", "tests/ci/test_ci.py",
                    "scripts/prune-worktrees.sh", "tests/agents/prune-worktrees-test.sh",
                    "tests/bench/walk-hash/Cargo.toml", "tests/e2e/run.sh",
                    "tests/registry/compose.yaml", "tests/rootless/lane.sh",
                    ".github/release.yml", ".gitignore",
                    ".github/ISSUE_TEMPLATE/task.yml", ".github/dependabot.yml"], set())

    def test_an_unknown_path_runs_everything(self):
        self.check(["newdir/file", "docs/testing.md"], ci.ALL, ["newdir/file"])
        # Only the named files under .github/ run nothing: a local action
        # or anything new there runs every lane.
        self.check([".github/actions/setup/action.yml"], ci.ALL, [".github/actions/setup/action.yml"])
        self.check([".github/CODEOWNERS"], ci.ALL, [".github/CODEOWNERS"])
        self.check(["Makefile"], ci.ALL, ["Makefile"])
        # `*` does not cross a `/`: a nested Markdown file outside docs/ is
        # not a top-level one.
        self.check(["other/README.md"], ci.ALL, ["other/README.md"])

    def test_nothing_changed_runs_nothing(self):
        self.check([], set())

    @unittest.skipUnless((ROOT / ".git").exists(), "not a git checkout")
    def test_every_tracked_file_has_a_rule(self):
        out = subprocess.run(["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True)
        paths = [p for p in out.stdout.decode("utf-8", "surrogateescape").split("\0") if p]
        self.assertGreater(len(paths), 100)
        missing = [p for p in paths if ci.lanes_of(p) is None]
        self.assertEqual(missing, [], "give these a rule in scripts/ci.py's RULES")


class Events(unittest.TestCase):
    def changes(self, paths, **env):
        with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
            f.write("\n".join(paths) + "\n")
        try:
            r = run(["changes", "--paths", f.name], env={f"CI_{k.upper()}": v for k, v in env.items()})
        finally:
            os.unlink(f.name)
        return r

    def pr(self, paths=("crates/x/src/lib.rs",), draft="false", labels="[]", **env):
        return self.changes(list(paths), event="pull_request", draft=draft, labels=labels, **env)

    def test_push_and_manual_runs_run_everything(self):
        for event in ["push", "workflow_dispatch"]:
            r = self.changes(["docs/testing.md"], event=event)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(lanes(r.stdout), set(ci.ALL))
            self.assertEqual(outputs(r.stdout)["full"], "true")
            self.assertEqual(outputs(r.stdout)["label_only"], "false")

    def test_a_draft_runs_the_fast_lane(self):
        r = self.pr(draft="true", action="synchronize")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(outputs(r.stdout)["full"], "false")
        self.assertEqual(lanes(r.stdout), RUST_T2)

    def test_ci_full_or_ready_runs_the_full_lane(self):
        r = self.pr(draft="true", labels='["agent", "ci:full"]', action="synchronize")
        self.assertEqual(outputs(r.stdout)["full"], "true")
        r = self.pr(draft="false", labels='["agent"]', action="ready_for_review")
        self.assertEqual(outputs(r.stdout)["full"], "true")

    def test_only_ci_full_on_a_draft_is_a_label_change_that_counts(self):
        cases = [
            # (action, label, draft, labels after the change, label_only, full)
            ("labeled", "agent", "true", '["agent"]', True, False),
            ("unlabeled", "area:ci", "true", "[]", True, False),
            ("labeled", "agent", "false", '["agent"]', True, False),
            ("labeled", "ci:full", "true", '["ci:full"]', False, True),
            ("unlabeled", "ci:full", "true", "[]", False, False),
            # Ready already runs everything: adding or removing ci:full
            # changes nothing.
            ("labeled", "ci:full", "false", '["ci:full"]', True, False),
            ("unlabeled", "ci:full", "false", "[]", True, False),
            # Case does not matter, as for GitHub's `==` in the concurrency
            # group.
            ("labeled", "CI:Full", "true", '["CI:Full"]', False, True),
            ("unlabeled", "CI:FULL", "true", "[]", False, False),
            ("labeled", "Agent", "true", '["Agent", "CI:FULL"]', True, False),
        ]
        for action, label, draft, labels, label_only, full in cases:
            r = self.pr(draft=draft, labels=labels, action=action, label=label)
            self.assertEqual(r.returncode, 0, r.stderr)
            out = outputs(r.stdout)
            what = (action, label, draft)
            self.assertEqual(out["label_only"], str(label_only).lower(), what)
            self.assertEqual(out["full"], str(full).lower(), what)
            if label_only:
                self.assertEqual(lanes(r.stdout), set(), what)
            else:
                self.assertEqual(lanes(r.stdout), RUST_T2, what)

    def test_bad_event_values_fail(self):
        self.assertNotEqual(self.pr(draft="yes").returncode, 0)
        self.assertNotEqual(self.pr(draft="").returncode, 0)
        self.assertNotEqual(self.pr(labels="agent").returncode, 0)
        self.assertNotEqual(self.pr(labels='{"name": "ci:full"}').returncode, 0)
        self.assertNotEqual(self.pr(labels='[1]').returncode, 0)
        r = run(["changes"])
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("CI_EVENT", r.stderr)

    def test_a_label_in_another_case_still_selects_the_full_lane(self):
        r = self.pr(draft="true", labels='["Ci:Full"]', action="synchronize")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(outputs(r.stdout)["full"], "true")

    def test_in_a_workflow_the_outputs_must_have_somewhere_to_go(self):
        for env in [{"CI_EVENT": "push"},
                    {"CI_EVENT": "pull_request", "CI_DRAFT": "false", "CI_LABELS": "[]"},
                    {"CI_EVENT": "pull_request", "CI_DRAFT": "true", "CI_LABELS": "[]",
                     "CI_ACTION": "labeled", "CI_LABEL": "agent"}]:
            r = run(["changes", "--paths", "-"], env=env, stdin="crates/x/src/lib.rs\n", github_output=False)
            self.assertNotEqual(r.returncode, 0, env)
            self.assertIn("GITHUB_OUTPUT", r.stderr, env)
            self.assertEqual(outputs(r.stdout), {}, env)

    def test_by_hand_a_list_stands_for_a_ready_pull_request(self):
        r = run(["changes", "--paths", "-"], stdin="docs/book/src/SUMMARY.md\n")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(lanes(r.stdout), {"book"})
        self.assertEqual(outputs(r.stdout)["full"], "true")

    def test_github_output_gets_the_same_lines(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "out"
            out.write_text("earlier=1\n")
            with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
                f.write("deny.toml\n")
            try:
                r = run(["changes", "--paths", f.name],
                        env={"CI_EVENT": "pull_request", "CI_DRAFT": "true", "CI_LABELS": "[]",
                             "GITHUB_OUTPUT": str(out)})
            finally:
                os.unlink(f.name)
            self.assertEqual(r.returncode, 0, r.stderr)
            written = out.read_text().splitlines()
            self.assertEqual(written[0], "earlier=1")
            self.assertEqual(dict(x.split("=") for x in written[1:]), outputs(r.stdout))
            self.assertEqual(sorted(outputs(r.stdout)), sorted([*ci.LANES, "full", "label_only"]))
            self.assertEqual(lanes(r.stdout), {"deny"})

    def test_a_hostile_path_cannot_write_a_workflow_command(self):
        hostile = ["::set-output name=rust::false", "a\n::error::x", "x ##[set-output name=t2;]false"]
        r = self.pr(paths=hostile, action="opened")
        self.assertEqual(r.returncode, 0, r.stderr)
        for line in (r.stdout + r.stderr).splitlines():
            self.assertFalse(line.lstrip().startswith("::") and "no rule in scripts/ci.py" not in line, line)
            self.assertNotIn("##[", line)
            self.assertNotIn("::set-output", line)
            self.assertNotIn("::error::x", line)
        # Unclassified: every lane runs.
        self.assertEqual(lanes(r.stdout), set(ci.ALL))


@unittest.skipUnless(subprocess.run(["git", "--version"], capture_output=True).returncode == 0, "no git")
class GitDiff(unittest.TestCase):
    def git(self, d, *args):
        subprocess.run(["git", "-C", d, *args], check=True, capture_output=True,
                       env={"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
                            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                            "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                            "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"})

    def head(self, d):
        return subprocess.run(["git", "-C", d, "rev-parse", "HEAD"], capture_output=True,
                              text=True, check=True).stdout.strip()

    def repo(self, d):
        self.git(d, "init", "-q", "-b", "main")
        (Path(d) / "crates" / "x").mkdir(parents=True)
        (Path(d) / "crates" / "x" / "notes.md").write_text("a\n" * 20)
        (Path(d) / "docs").mkdir()
        (Path(d) / "docs" / "a.md").write_text("a\n")
        self.git(d, "add", "-A")
        self.git(d, "commit", "-q", "-m", "base")
        return self.head(d)

    def diff_run(self, d, base, head):
        return run(["changes"], cwd=d, env={
            "CI_EVENT": "pull_request", "CI_DRAFT": "false", "CI_LABELS": "[]",
            "CI_ACTION": "synchronize", "CI_BASE": base, "CI_HEAD": head})

    def test_a_rename_counts_both_paths(self):
        with tempfile.TemporaryDirectory() as d:
            base = self.repo(d)
            self.git(d, "mv", "crates/x/notes.md", "docs/notes.md")
            self.git(d, "commit", "-q", "-m", "move")
            r = self.diff_run(d, base, self.head(d))
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(lanes(r.stdout), RUST_T2)

    def test_only_the_heads_changes_count(self):
        with tempfile.TemporaryDirectory() as d:
            base = self.repo(d)
            self.git(d, "switch", "-q", "-c", "topic")
            (Path(d) / "docs" / "a.md").write_text("b\n")
            self.git(d, "commit", "-qam", "docs")
            head = self.head(d)
            # main moves on with a Rust change the pull request does not make.
            self.git(d, "switch", "-q", "main")
            (Path(d) / "crates" / "x" / "notes.md").write_text("c\n")
            self.git(d, "commit", "-qam", "rust on main")
            r = self.diff_run(d, self.head(d), head)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(lanes(r.stdout), set())
            self.assertIn("1 changed path", r.stdout)

    def test_a_commit_git_does_not_have_runs_everything(self):
        with tempfile.TemporaryDirectory() as d:
            base = self.repo(d)
            r = self.diff_run(d, base, "f" * 40)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(lanes(r.stdout), set(ci.ALL))
            self.assertIn("::warning::git diff", r.stdout)

    def test_a_base_that_is_no_hash_fails(self):
        with tempfile.TemporaryDirectory() as d:
            base = self.repo(d)
            for bad in ["--output=/tmp/x", "HEAD", "", base[:12]]:
                r = self.diff_run(d, bad, base)
                self.assertNotEqual(r.returncode, 0, bad)
                self.assertIn("not a commit hash", r.stderr)


def cache(i, key, created, ref="refs/heads/main"):
    return {"id": i, "key": key, "ref": ref, "createdAt": created}


class StaleCaches(unittest.TestCase):
    ENTRIES = [
        cache(1, "v0-rust-test-Linux-x64-a74c6f26-66067b68", "2026-10-29T22:22:11.1Z"),
        cache(2, "v0-rust-test-Linux-x64-a74c6f26-e38b0365", "2026-11-03T03:28:52.123456Z"),
        cache(3, "v0-rust-test-Linux-x64-a74c6f26-78d2288a", "2026-11-01T03:21:12Z"),
        # Another environment hash (toolchain) is another restore key.
        cache(4, "v0-rust-test-Linux-x64-0000abcd-11111111", "2026-10-01T00:00:00Z"),
        cache(5, "v0-rust-test-aarch64-Linux-arm64-0203a50a-e38b0365", "2026-11-03T03:30:11Z"),
        cache(6, "v0-rust-test-aarch64-Linux-arm64-0203a50a-f4c55bcb", "2026-10-30T12:44:17Z"),
        cache(7, "v0-rust-t2-unit-c-unit-c-Linux-x64-a74c6f26-e38b0365", "2026-11-03T15:57:33Z"),
        cache(8, "v0-rust-t2-unit-c-unit-c-Linux-x64-a74c6f26-00000000", "2026-11-02T15:57:33Z"),
        # Not main's, not rust-cache's, or not a named job's: never touched.
        cache(9, "v0-rust-test-Linux-x64-a74c6f26-00000001", "2026-10-01T00:00:00Z", "refs/pull/7/merge"),
        cache(10, "setup-zig-tarball-zig-x86_64-linux-0.16.0", "2026-10-29T21:11:38Z"),
        cache(11, "v0-rust-check-aarch64-Linux-x64-a74c6f26-9e647eda", "2026-10-29T22:08:15Z"),
        cache(12, "v0-rust-check-aarch64-Linux-x64-a74c6f26-a8cd6c36", "2026-11-01T08:59:39Z"),
    ]

    def stale(self, names, entries=None):
        r = run(["stale-caches", *names], stdin=json.dumps(self.ENTRIES if entries is None else entries))
        self.assertEqual(r.returncode, 0, r.stderr)
        return [int(line.split("\t")[0]) for line in r.stdout.splitlines()]

    def test_all_but_the_newest_of_each_restore_key(self):
        self.assertEqual(sorted(self.stale(["test", "test-aarch64"])), [1, 3, 6])
        self.assertEqual(self.stale(["t2-unit-c-unit-c"]), [8])
        self.assertEqual(self.stale(["clippy"]), [])

    def test_lines_name_the_key(self):
        r = run(["stale-caches", "test-aarch64"], stdin=json.dumps(self.ENTRIES))
        self.assertEqual(r.stdout, "6\tv0-rust-test-aarch64-Linux-arm64-0203a50a-f4c55bcb\n")

    def test_the_same_time_is_broken_by_id(self):
        entries = [cache(20, "v0-rust-docs-Linux-x64-a74c6f26-00000002", "2026-11-01T00:00:00Z"),
                   cache(21, "v0-rust-docs-Linux-x64-a74c6f26-00000003", "2026-11-01T00:00:00Z")]
        self.assertEqual(self.stale(["docs"], entries), [20])

    def test_bad_input_fails(self):
        for stdin in ["", "{}", "[1]", json.dumps([{"id": "1", "key": "k", "ref": "r", "createdAt": "t"}]),
                      json.dumps([cache(1, "v0-rust-test-Linux-x64-a74c6f26-00000002", "yesterday"),
                                  cache(2, "v0-rust-test-Linux-x64-a74c6f26-00000003", "today")])]:
            r = run(["stale-caches", "test"], stdin=stdin)
            self.assertNotEqual(r.returncode, 0, stdin)
        self.assertNotEqual(run(["stale-caches"], stdin="[]").returncode, 0)
        self.assertNotEqual(run(["stale-caches", "a b"], stdin="[]").returncode, 0)


def jobs(workflow: str) -> dict[str, list[str]]:
    """The lines of each job of a workflow, by job id (enough YAML for these
    files: job ids at two spaces under `jobs:`)."""
    lines = (WORKFLOWS / workflow).read_text().splitlines()
    found: dict[str, list[str]] = {}
    current = None
    for line in lines[lines.index("jobs:") + 1:]:
        m = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line)
        if m:
            current = m.group(1)
            found[current] = []
        elif current is not None:
            found[current].append(line)
    return found


def needs(lines: list[str]) -> list[str]:
    for line in lines:
        m = re.fullmatch(r"    needs: \[(.*)\]", line) or re.fullmatch(r"    needs: (\S+)", line)
        if m:
            return [x.strip() for x in m.group(1).split(",")]
    return []


def cache_name(job: str, lines: list[str]) -> str | None:
    """The name rust-cache gives the job's entries: the shared key, else the
    key and the job id, else the job id; None without rust-cache."""
    for i, line in enumerate(lines):
        if re.match(r"\s+- uses: Swatinem/rust-cache@", line):
            indent = len(line) - len(line.lstrip())
            keys = {}
            for nxt in lines[i + 1:]:
                if not nxt.strip() or nxt.strip().startswith("#"):
                    continue
                if len(nxt) - len(nxt.lstrip()) <= indent:
                    break
                m = re.fullmatch(r"\s+(shared-key|key): (\S+)", nxt)
                if m:
                    keys[m.group(1)] = m.group(2)
            if "shared-key" in keys:
                return keys["shared-key"]
            return f"{keys['key']}-{job}" if "key" in keys else job
    return None


def job_line(lines: list[str], key: str) -> str | None:
    """The value of a job-level key (`if`, `name`), or None."""
    for line in lines:
        m = re.fullmatch(rf"    {key}: (.*)", line)
        if m:
            return m.group(1)
    return None


RUST = "needs.changes.outputs.rust == 'true'"
FULL = "needs.changes.outputs.full == 'true'"
T2 = "needs.changes.outputs.t2 == 'true'"
PUSH_TO_MAIN = "always() && github.event_name == 'push' && github.ref == 'refs/heads/main'"


def gate_name(base: str, draft: str) -> str:
    return (
        "${{ needs.changes.outputs.label_only == 'true' && '" + base + " (label change, nothing run)'"
        " || github.event_name == 'workflow_dispatch' && github.ref != 'refs/heads/main' && '" + base
        + " (manual run)' || needs.changes.outputs.full == 'false' && '" + base + " (" + draft + ")'"
        " || '" + base + "' }}"
    )


# Each job's exact `if:` and each gate's `name:`: a change to the lanes'
# wiring has to change this table too.
CONDITIONS = {
    "ci.yml": {
        "changes": None,
        "fmt": RUST,
        "clippy": RUST,
        "test": RUST,
        "static-build": f"{RUST} && {FULL}",
        "test-aarch64": f"{RUST} && {FULL}",
        "deny": "needs.changes.outputs.deny == 'true'",
        "docs": f"{RUST} || needs.changes.outputs.book == 'true'",
        "docs-lint": "needs.changes.outputs.label_only != 'true'",
        "caches": PUSH_TO_MAIN,
        "result": "always()",
    },
    "t2.yml": {
        "changes": None,
        "unit": f"{T2} && {FULL}",
        "unit-c": f"{T2} && {FULL}",
        "smoke": f"{T2} && {FULL}",
        "k8s": f"{T2} && {FULL}",
        "caches": PUSH_TO_MAIN,
        "result": "always()",
    },
}
GATE_NAMES = {
    "ci.yml": gate_name("CI result", "draft, fast lane"),
    "t2.yml": gate_name("T2 result", "draft, not run"),
}
# The outputs each workflow's `changes` job maps from its `lanes` step, in
# order. The runner drops an output whose value is empty, so a misspelt
# mapping makes a key vanish: the gates check these keys by name.
CHANGES_OUTPUTS = {
    "ci.yml": ["rust", "book", "deny", "full", "label_only"],
    "t2.yml": ["t2", "full", "label_only"],
}


def outputs_check(keys: list[str]) -> str:
    names = ",".join(f'"{k}"' for k in keys)
    return f"jq -e '.changes.outputs as $o | [{names}] | all($o[.] == \"true\" or $o[.] == \"false\")'"


def block(lines: list[str], header: str, indent: str) -> list[str]:
    """The lines right under `header` indented by exactly `indent`."""
    start = lines.index(header) + 1
    out = []
    for line in lines[start:]:
        if not line.startswith(indent) or line.startswith(indent + " "):
            break
        out.append(line)
    return out


class Workflows(unittest.TestCase):
    LANED = ["ci.yml", "t2.yml"]

    def test_every_jobs_condition_and_the_gates_names_are_pinned(self):
        for wf in self.LANED:
            js = jobs(wf)
            self.assertEqual(sorted(js), sorted(CONDITIONS[wf]), wf)
            for job, want in CONDITIONS[wf].items():
                self.assertEqual(job_line(js[job], "if"), want, f"{wf}: {job}")
            self.assertEqual(job_line(js["result"], "name"), GATE_NAMES[wf], wf)

    def test_the_gates_check_the_lanes_outputs_by_name(self):
        for wf in self.LANED:
            script = "\n".join(jobs(wf)["result"])
            self.assertIn(outputs_check(CHANGES_OUTPUTS[wf]), script, wf)
            self.assertIn(".changes.result", script, wf)

    def test_the_changes_outputs_are_pinned(self):
        emitted = set(ci.LANES) | {"full", "label_only"}
        for wf in self.LANED:
            lines = jobs(wf)["changes"]
            keys = CHANGES_OUTPUTS[wf]
            self.assertLessEqual(set(keys), emitted, wf)
            self.assertEqual(block(lines, "    outputs:", "      "),
                             [f"      {k}: ${{{{ steps.lanes.outputs.{k} }}}}" for k in keys], wf)
            # The step those outputs name is the one that runs the script.
            self.assertEqual(lines.count("        id: lanes"), 1, wf)
            step = lines[lines.index("        id: lanes"):]
            step = step[:next((i for i, x in enumerate(step) if x.startswith("      - ")), len(step))]
            self.assertIn("        run: python3 -I scripts/ci.py changes", step, wf)
            # Every output the workflow reads is one the job maps.
            text = (WORKFLOWS / wf).read_text()
            read = set(re.findall(r"needs\.changes\.outputs\.([A-Za-z0-9_-]+)", text))
            self.assertLessEqual(read, set(keys), wf)

    def test_the_gate_needs_every_job_and_always_runs(self):
        for wf in self.LANED:
            js = jobs(wf)
            self.assertIn("result", js, wf)
            want = sorted(j for j in js if j not in ("result", "caches"))
            self.assertEqual(sorted(needs(js["result"])), want, wf)
            # Not `!cancelled()`: a cancelled run's gate must fail, not be
            # skipped (ADR-0095).
            self.assertIn("    if: always()", js["result"], wf)

    def test_every_tested_job_waits_for_the_lanes(self):
        for wf in self.LANED:
            for job, lines in jobs(wf).items():
                if job not in ("changes", "caches"):
                    self.assertIn("changes", needs(lines), f"{wf}: {job}")

    def test_the_cache_pruning_names_exactly_the_jobs_that_cache(self):
        for wf in self.LANED:
            js = jobs(wf)
            names = {j: cache_name(j, lines) for j, lines in js.items()}
            caching = sorted(j for j, n in names.items() if n)
            self.assertEqual(sorted(needs(js["caches"])), caching, wf)
            run_line = next(x for x in js["caches"] if "scripts/ci.py stale-caches" in x)
            listed = re.search(r"stale-caches ([A-Za-z0-9 _-]+) <", run_line).group(1).split()
            self.assertEqual(sorted(listed), sorted({names[j] for j in caching}), wf)
            self.assertIn("    if: always() && github.event_name == 'push' && github.ref == 'refs/heads/main'",
                          js["caches"], wf)

    def test_the_t2_jobs_that_build_the_same_share_their_cache(self):
        js = jobs("t2.yml")
        self.assertEqual(cache_name("unit", js["unit"]), "t2-tests")
        self.assertEqual(cache_name("unit-c", js["unit-c"]), "t2-tests")
        self.assertEqual(cache_name("smoke", js["smoke"]), "t2-smoke")
        self.assertEqual(cache_name("k8s", js["k8s"]), "t2-smoke")

    def test_both_workflows_have_the_same_triggers_and_label_rule(self):
        def picked(wf):
            text = (WORKFLOWS / wf).read_text()
            return (re.search(r"(?m)^    types: .*$", text).group(0),
                    re.search(r"(?m)^  group: .*$", text).group(0))
        self.assertEqual(picked("ci.yml"), picked("t2.yml"))
        types, group = picked("ci.yml")
        self.assertEqual(types, "    types: [opened, synchronize, reopened, ready_for_review, labeled, unlabeled]")
        # scripts/ci.py's label_only, as a GitHub expression.
        self.assertIn("(github.event.action == 'labeled' || github.event.action == 'unlabeled')"
                      " && !(github.event.label.name == 'ci:full' && github.event.pull_request.draft)", group)
        self.assertEqual(ci.FULL_LABEL, "ci:full")

    def test_the_security_terms_hold(self):
        for wf in ["ci.yml", "t2.yml", "release.yml"]:
            text = (WORKFLOWS / wf).read_text()
            self.assertNotRegex(text, r"(?m)^\s+pull_request_target\b", wf)
            self.assertNotRegex(text, r"\$\{\{[^}]*\bsecrets\.", wf)
            self.assertIn("\npermissions:\n  contents: read\n", text, wf)
            uses = re.findall(r"uses: (\S+)", text)
            self.assertTrue(uses, wf)
            for u in uses:
                self.assertRegex(u, r"^[\w.-]+/[\w./-]+@[0-9a-f]{40}$", f"{wf}: pin {u} by its commit")
            self.assertEqual(text.count("uses: actions/checkout@"), text.count("persist-credentials: false"), wf)
            for job, lines in jobs(wf).items():
                self.assertTrue(any(re.fullmatch(r"    timeout-minutes: \d+", x) for x in lines), f"{wf}: {job}")

    def test_no_laned_workflow_filters_paths(self):
        for wf in self.LANED:
            self.assertNotRegex((WORKFLOWS / wf).read_text(), r"(?m)^\s+paths(-ignore)?:", wf)


if __name__ == "__main__":
    unittest.main(verbosity=1)
