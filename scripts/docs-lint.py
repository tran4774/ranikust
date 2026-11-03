#!/usr/bin/env python3
r"""Mechanical checks on the decision records (`just docs-lint`).

- every ADR file is listed in docs/adr/README.md, and every listed ADR exists;
- an ADR's front-matter status matches its status in the index;
- no question number appears twice in docs/open-questions.md (the answered
  archive and the headings of the questions moved to issues);
- no `[open: Q-NNN]` marker remains outside ADR-0001 to ADR-0089, the
  accepted ADRs written before the move, which are never rewritten: open
  questions are GitHub issues since ADR-0088, named by their number since
  ADR-0091, and their markers are `[open: #N]`;
- every `[open: #N]` marker in code or docs names an open issue (not a pull
  request) labelled `question`; inside docs/adr/ the issue only has to be a
  `question` issue, open or closed, since an accepted ADR is not rewritten
  when its question is answered. GitHub is asked through `gh` (one
  paginated call). In rustdoc comments the marker is written `\[open: #N]`
  so rustdoc does not read it as a link; the check finds it either way.

The files checked are the ones `git ls-files` lists (tracked, plus untracked
ones not ignored), so agent worktrees, `target/` and other ignored trees are
never read.

Without `gh`, without a logged-in `gh`, or when the network fails, the
issue check is skipped and the verdict line says so; set
DOCS_LINT_REQUIRE_GITHUB=1 to make a skip a problem. Any other `gh` failure
(an HTTP error such as 404 or 401, a rate limit, unexpected output) is a
problem.

Prints every problem and exits 1 if there is any. Standard library, `git`
and `gh` only; run from the repository root.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path.cwd()
REPO = "tran4774/ranikust"
ADR_DIR = ROOT / "docs" / "adr"
INDEX = ADR_DIR / "README.md"
QUESTIONS = ROOT / "docs" / "open-questions.md"
# Where `[open: ...]` markers may live: (directory prefix, file suffix).
MARKER_FILES = (
    ("crates/", ".rs"),
    ("crates/", "/Cargo.toml"),
    ("docs/", ".md"),
    ("tests/", ".sh"),
    ("tests/", ".py"),
    (".claude/agents/", ".md"),
    (".claude/skills/", ".md"),
    (".github/", ".md"),
    (".github/", ".yml"),
)
# Top-level files with markers, besides every top-level `*.md`.
MARKER_TOP_LEVEL = ("Cargo.toml", "justfile")
# Frozen, generated or foreign material that may quote old markers.
MARKER_SKIP = ("docs/research/", "target/", "graphify-out/", ".claude/worktrees/")
# Accepted ADRs are never rewritten, so the ones written before the move to
# issues keep their old markers (ADR-0088); later ADRs use `[open: #N]`.
LAST_ADR_WITH_OLD_MARKERS = 89
ADR_PATH = re.compile(r"^docs/adr/(\d{4})-[^/]+\.md$")
# `gh` failures that mean "no network": the issue check is skipped.
NETWORK_ERROR = re.compile(
    r"error connecting to|dial tcp|no such host|could not resolve|connection refused"
    r"|connection reset|i/o timeout|TLS handshake timeout|network is unreachable"
    r"|proxyconnect",
    re.I,
)
# `gh` with no account to use: the issue check is skipped too.
NOT_LOGGED_IN = re.compile(r"gh auth login|not logged in", re.I)

ADR_FILE = re.compile(r"^(\d{4})-[a-z0-9-]+\.md$")
INDEX_ROW = re.compile(r"^\|\s*\[(\d{4})\]\(([^)]+)\)\s*\|(?:[^|]*\|)\s*([A-Za-z ]+?)\s*\|")
STATUS = re.compile(r"^status:\s*([a-z-]+)", re.M)
QUESTION = re.compile(r"^### (Q-\d+):", re.M)
OLD_MARKER = re.compile(r"\[open: (Q-\d+)\]")
MARKER = re.compile(r"\[open: #(\d+)\]")


def adr_checks(problems):
    files = {}
    for path in sorted(ADR_DIR.glob("[0-9][0-9][0-9][0-9]-*.md")):
        m = ADR_FILE.match(path.name)
        if not m:
            problems.append(f"{path.relative_to(ROOT)}: name is not NNNN-title-with-dashes.md")
            continue
        status = STATUS.search(path.read_text(encoding="utf-8"))
        files[m.group(1)] = (path.name, status.group(1) if status else None)
        if not status:
            problems.append(f"{path.relative_to(ROOT)}: no `status:` in the front matter")
    listed = {}
    for line in INDEX.read_text(encoding="utf-8").splitlines():
        m = INDEX_ROW.match(line)
        if m:
            num, target, status = m.groups()
            if num in listed:
                problems.append(f"docs/adr/README.md: ADR-{num} listed twice")
            listed[num] = (target, status.strip().lower())
    for num, (name, status) in files.items():
        if num not in listed:
            problems.append(f"docs/adr/{name}: not listed in docs/adr/README.md")
            continue
        target, index_status = listed[num]
        if target != name:
            problems.append(f"docs/adr/README.md: ADR-{num} links to {target}, the file is {name}")
        # The index may qualify the status ("Superseded by ADR-0050"); its
        # first word must match the front matter.
        if status and index_status.split()[0] != status.split("-")[0]:
            problems.append(
                f"ADR-{num}: front matter says `{status}`, the index says `{index_status}`"
            )
    for num in listed:
        if num not in files:
            problems.append(f"docs/adr/README.md: ADR-{num} has no file")


def old_markers_allowed(rel):
    """Only the accepted ADRs written before the move to issues keep `[open: Q-NNN]`."""
    m = ADR_PATH.match(rel)
    return bool(m) and int(m.group(1)) <= LAST_ADR_WITH_OLD_MARKERS


def is_marker_file(rel):
    if rel.startswith(MARKER_SKIP) or rel == "docs/open-questions.md":
        return False
    if "/" not in rel:
        return rel in MARKER_TOP_LEVEL or rel.endswith(".md")
    return any(rel.startswith(d) and rel.endswith(s) for d, s in MARKER_FILES)


def marker_files(problems):
    """The repository's files that may hold markers, as `git ls-files` lists them."""
    try:
        out = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            capture_output=True, timeout=60, check=True,
        )
    except (OSError, subprocess.SubprocessError) as e:
        problems.append(f"cannot list the repository's files with git ls-files: {e}")
        return
    for rel in sorted({p for p in out.stdout.decode("utf-8", "replace").split("\0") if p}):
        if not is_marker_file(rel):
            continue
        try:
            yield rel, (ROOT / rel).read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError):
            continue


def github_issues():
    """Asks GitHub for the repository's issues, pull requests left out.

    Returns (issues, skip, error): `issues` maps a number to (state, labels);
    `skip` says why the check cannot run (no `gh`, `gh` not logged in, no
    network); `error` is any other failure, which is a problem.
    """
    try:
        out = subprocess.run(
            ["gh", "api", "--paginate", f"repos/{REPO}/issues?state=all&per_page=100",
             "--jq", ".[] | [.number, .state, (.pull_request != null), [.labels[].name]] | @json"],
            capture_output=True, text=True, timeout=120, check=False,
        )
    except FileNotFoundError:
        return None, "gh is not installed", None
    except subprocess.TimeoutExpired:
        return None, "gh timed out", None
    except OSError as e:
        return None, None, f"gh could not run: {e}"
    if out.returncode != 0:
        err = out.stderr.strip() or f"gh exited with {out.returncode}"
        last = err.splitlines()[-1]
        if NETWORK_ERROR.search(err):
            return None, f"network error: {last}", None
        if NOT_LOGGED_IN.search(err):
            return None, "gh is not logged in (run `gh auth login`)", None
        return None, None, f"gh api repos/{REPO}/issues failed: {last}"
    issues = {}
    try:
        for line in out.stdout.splitlines():
            number, state, is_pr, labels = json.loads(line)
            if not is_pr:
                issues[number] = (state, labels)
    except (ValueError, TypeError) as e:
        return None, None, f"unexpected output from gh: {e}"
    return issues, None, None


def question_checks(problems):
    """Returns why the issue check was skipped, or None."""
    text = QUESTIONS.read_text(encoding="utf-8")
    seen = {}
    for q in QUESTION.findall(text):
        seen[q] = seen.get(q, 0) + 1
    for q, n in sorted(seen.items()):
        if n > 1:
            problems.append(f"docs/open-questions.md: {q} appears {n} times")
    markers = []
    for rel, lines in marker_files(problems):
        for i, line in enumerate(lines, 1):
            if not old_markers_allowed(rel):
                for q in OLD_MARKER.findall(line):
                    problems.append(
                        f"{rel}:{i}: [open: {q}] is the old form: open questions are issues "
                        "(ADR-0088, ADR-0091), mark the place with [open: #N]"
                    )
            markers += [(rel, i, int(n)) for n in MARKER.findall(line)]
    if not markers:
        return None
    issues, skip, error = github_issues()
    if error:
        problems.append(error)
        return None
    if skip:
        if os.environ.get("DOCS_LINT_REQUIRE_GITHUB") == "1":
            problems.append(f"the issue check could not run: {skip}")
            return None
        return skip
    for rel, i, n in markers:
        if n not in issues:
            problems.append(f"{rel}:{i}: [open: #{n}] names no issue (or a pull request)")
            continue
        state, labels = issues[n]
        if "question" not in labels:
            problems.append(f"{rel}:{i}: [open: #{n}] but issue #{n} is not labelled `question`")
        elif state != "open" and not rel.startswith("docs/adr/"):
            # An accepted ADR is not rewritten when its question is answered.
            problems.append(f"{rel}:{i}: [open: #{n}] but issue #{n} is {state}")
    return None


def main():
    problems = []
    adr_checks(problems)
    skipped = question_checks(problems)
    for p in problems:
        print(p)
    if problems:
        print(f"docs-lint: {len(problems)} problem(s)", file=sys.stderr)
        sys.exit(1)
    print(f"docs-lint: ok (issue check skipped: {skipped})" if skipped else "docs-lint: ok")


if __name__ == "__main__":
    main()
