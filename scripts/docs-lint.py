#!/usr/bin/env python3
"""Mechanical checks on the decision records (`just docs-lint`).

- every ADR file is listed in docs/adr/README.md, and every listed ADR exists;
- an ADR's front-matter status matches its status in the index;
- no question number appears twice in docs/open-questions.md;
- no `[open: Q-NNN]` marker remains, in code or docs, for a question that is
  answered, and none names a question that does not exist.

Prints every problem and exits 1 if there is any. Standard library only;
run from the repository root.
"""

import re
import sys
from pathlib import Path

ROOT = Path.cwd()
ADR_DIR = ROOT / "docs" / "adr"
INDEX = ADR_DIR / "README.md"
QUESTIONS = ROOT / "docs" / "open-questions.md"
# Where `[open: Q-NNN]` markers may live.
MARKER_GLOBS = ["crates/**/*.rs", "docs/**/*.md", "*.md", "tests/**/*.sh", "tests/**/*.py"]
# Frozen or generated material that may quote old markers.
MARKER_SKIP = ("docs/research/", "target/", "graphify-out/")

ADR_FILE = re.compile(r"^(\d{4})-[a-z0-9-]+\.md$")
INDEX_ROW = re.compile(r"^\|\s*\[(\d{4})\]\(([^)]+)\)\s*\|(?:[^|]*\|)\s*([A-Za-z ]+?)\s*\|")
STATUS = re.compile(r"^status:\s*([a-z-]+)", re.M)
QUESTION = re.compile(r"^### (Q-\d+):", re.M)
MARKER = re.compile(r"\[open: (Q-\d+)\]")


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


def question_checks(problems):
    text = QUESTIONS.read_text(encoding="utf-8")
    head, _, answered = text.partition("\n## Answered")
    open_qs = QUESTION.findall(head)
    answered_qs = QUESTION.findall(answered)
    seen = {}
    for q in open_qs + answered_qs:
        seen[q] = seen.get(q, 0) + 1
    for q, n in sorted(seen.items()):
        if n > 1:
            problems.append(f"docs/open-questions.md: {q} appears {n} times")
    known = set(seen)
    answered_set = set(answered_qs) - set(open_qs)
    for pattern in MARKER_GLOBS:
        for path in ROOT.glob(pattern):
            rel = path.relative_to(ROOT).as_posix()
            if rel.startswith(MARKER_SKIP) or rel == "docs/open-questions.md":
                continue
            try:
                lines = path.read_text(encoding="utf-8").splitlines()
            except (OSError, UnicodeDecodeError):
                continue
            for i, line in enumerate(lines, 1):
                for q in MARKER.findall(line):
                    if q in answered_set:
                        problems.append(f"{rel}:{i}: [open: {q}] but {q} is answered")
                    elif q not in known:
                        problems.append(f"{rel}:{i}: [open: {q}] names no question")


def main():
    problems = []
    adr_checks(problems)
    question_checks(problems)
    for p in problems:
        print(p)
    if problems:
        print(f"docs-lint: {len(problems)} problem(s)", file=sys.stderr)
        sys.exit(1)
    print("docs-lint: ok")


if __name__ == "__main__":
    main()
