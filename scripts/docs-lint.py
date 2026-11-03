#!/usr/bin/env python3
"""Mechanical checks on the decision records (`just docs-lint`).

- every ADR file is listed in docs/adr/README.md, and every listed ADR exists;
- an ADR's front-matter status matches its status in the index;
- no question number appears twice in the archive, docs/open-questions.md;
- no `[open: Q-NNN]` marker remains: open questions are GitHub issues since
  ADR-0088, and their markers are `[open: #N]`;
- every `[open: #N]` marker, in code or docs, names an open issue (not a
  pull request) of the repository, which is asked of the GitHub API with
  `gh` (one paginated call).

Offline, or without `gh`, the issue check is skipped with a warning and the
other checks still run; set DOCS_LINT_REQUIRE_GITHUB=1 to make a skipped
issue check a problem.

Prints every problem and exits 1 if there is any. Standard library and `gh`
only; run from the repository root.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path.cwd()
ADR_DIR = ROOT / "docs" / "adr"
INDEX = ADR_DIR / "README.md"
QUESTIONS = ROOT / "docs" / "open-questions.md"
# Where `[open: ...]` markers may live.
MARKER_GLOBS = [
    "crates/**/*.rs",
    "crates/**/Cargo.toml",
    "Cargo.toml",
    "justfile",
    "docs/**/*.md",
    "*.md",
    "tests/**/*.sh",
    "tests/**/*.py",
    ".claude/**/*.md",
    ".github/**/*.md",
    ".github/**/*.yml",
]
# Frozen or generated material that may quote old markers.
MARKER_SKIP = ("docs/research/", "target/", "graphify-out/")

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


def marker_files():
    seen = set()
    for pattern in MARKER_GLOBS:
        for path in ROOT.glob(pattern):
            rel = path.relative_to(ROOT).as_posix()
            if rel in seen or rel.startswith(MARKER_SKIP) or rel == "docs/open-questions.md":
                continue
            seen.add(rel)
            try:
                yield rel, path.read_text(encoding="utf-8").splitlines()
            except (OSError, UnicodeDecodeError):
                continue


def github_issues():
    """Maps issue number -> state ("open"/"closed"), pull requests left out.

    Returns (None, reason) when GitHub cannot be asked.
    """
    try:
        out = subprocess.run(
            ["gh", "api", "--paginate", "repos/{owner}/{repo}/issues?state=all&per_page=100",
             "--jq", ".[] | [.number, .state, (.pull_request != null)] | @json"],
            capture_output=True, text=True, timeout=120, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return None, str(e)
    if out.returncode != 0:
        return None, out.stderr.strip().splitlines()[-1] if out.stderr.strip() else "gh failed"
    issues = {}
    for line in out.stdout.splitlines():
        number, state, is_pr = json.loads(line)
        if not is_pr:
            issues[number] = state
    return issues, None


def question_checks(problems, warnings):
    text = QUESTIONS.read_text(encoding="utf-8")
    seen = {}
    for q in QUESTION.findall(text):
        seen[q] = seen.get(q, 0) + 1
    for q, n in sorted(seen.items()):
        if n > 1:
            problems.append(f"docs/open-questions.md: {q} appears {n} times")
    markers = []
    for rel, lines in marker_files():
        for i, line in enumerate(lines, 1):
            for q in OLD_MARKER.findall(line):
                problems.append(
                    f"{rel}:{i}: [open: {q}] is the old form: open questions are issues "
                    "(ADR-0088), mark the place with [open: #N]"
                )
            markers += [(rel, i, int(n)) for n in MARKER.findall(line)]
    if not markers:
        return
    issues, reason = github_issues()
    if issues is None:
        message = f"skipped the check of {len(markers)} [open: #N] marker(s): {reason}"
        if os.environ.get("DOCS_LINT_REQUIRE_GITHUB") == "1":
            problems.append(message)
        else:
            warnings.append(message)
        return
    for rel, i, n in markers:
        state = issues.get(n)
        if state is None:
            problems.append(f"{rel}:{i}: [open: #{n}] names no issue (or a pull request)")
        elif state != "open":
            problems.append(f"{rel}:{i}: [open: #{n}] but issue #{n} is {state}")


def main():
    problems, warnings = [], []
    adr_checks(problems)
    question_checks(problems, warnings)
    for w in warnings:
        print(f"docs-lint: warning: {w}", file=sys.stderr)
    for p in problems:
        print(p)
    if problems:
        print(f"docs-lint: {len(problems)} problem(s)", file=sys.stderr)
        sys.exit(1)
    print("docs-lint: ok")


if __name__ == "__main__":
    main()
