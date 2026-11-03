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
  so rustdoc does not read it as a link; the check finds it either way;
- every relative link in Markdown (inline, reference definition, image or
  HTML `href`/`src`), and every link to this repository on github.com
  (`blob/main`, `tree/main`, the front page's anchors), names a file that
  `git ls-files` lists and, for a Markdown file, a heading or `<a name|id>`
  anchor it has, by GitHub's rules; a link from the book stays inside
  docs/book/src, the only part mdBook publishes; `[text][label]` has its
  definition; a Markdown path named in a code span or in code
  (`docs/agents/rules.md#writing` in a Rust comment) resolves the same way,
  from the repository root or the naming file's directory. A frozen record
  cannot be edited, so a link from one that breaks is fixed at its target
  with an `<a name>` alias, and the problem says so.

The files checked are the ones `git ls-files` lists (tracked, plus untracked
ones not ignored), so agent worktrees, `target/` and other ignored trees are
never read.

Without `gh`, without a logged-in `gh`, or when the network fails, the
issue check is skipped and the verdict line says so. A response cut short
(gh's "unexpected end of JSON input", or a zero exit with output that is not
JSON) is retried once after 2 s and, cut short again, skipped the same way.
Set DOCS_LINT_REQUIRE_GITHUB=1 to make a skip a problem. Any other `gh`
failure (an HTTP error such as 404 or 401, a rate limit, output of the
wrong shape) is a problem.

Prints every problem and exits 1 if there is any. Standard library, `git`
and `gh` only; run from the repository root.
"""

import html
import json
import os
import posixpath
import re
import subprocess
import sys
import time
from pathlib import Path, PurePosixPath
from urllib.parse import unquote

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
NOT_LOGGED_IN = re.compile(r"To get started with GitHub CLI")
# A response cut short: retried once, then the issue check is skipped.
TRUNCATED = "unexpected end of JSON input"
RETRY_DELAY = 2.0

ADR_FILE = re.compile(r"^(\d{4})-[a-z0-9-]+\.md$")
INDEX_ROW = re.compile(r"^\|\s*\[(\d{4})\]\(([^)]+)\)\s*\|(?:[^|]*\|)\s*([A-Za-z ]+?)\s*\|")
STATUS = re.compile(r"^status:\s*([a-z-]+)", re.M)
QUESTION = re.compile(r"^### (Q-\d+):", re.M)
OLD_MARKER = re.compile(r"\[open: (Q-\d+)\]")
MARKER = re.compile(r"\[open: #(\d+)\]")

# Link checks. Templates hold placeholder links by design; the link check's
# own tests hold broken ones on purpose.
LINK_SKIP = ("docs/adr/template.md", "docs/research/TEMPLATE.md", "tests/docs/")
FROZEN = ("docs/research/", "docs/agents/phase-1.md", "docs/agents/phase-2.md",
          "docs/security/audit/", "docs/open-questions.md")
BOOK_SRC = "docs/book/src/"
CODE_SUFFIXES = (".rs", ".sh", ".py", ".toml", ".yml", ".yaml")
REPO_URL = re.compile(r"^https://github\.com/" + re.escape(REPO)
                      + r"/(?:blob|tree)/main/(?P<path>[^#?]+)(?:\?[^#]*)?(?:#(?P<frag>.*))?$")
REPO_HOME = re.compile(r"^https://github\.com/" + re.escape(REPO) + r"/?(?P<path>)(?:#(?P<frag>.*))?$")
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
GITHUB_LINE = re.compile(r"^L\d+(?:-L\d+)?$")
# `[text](dest "title")`, the text possibly holding an image or brackets.
INLINE_LINK = re.compile(
    r"\[(?:[^\[\]]|\[[^\]]*\])*\]\(\s*(?:<(?P<angle>[^>\n]*)>|(?P<plain>[^\s()]+(?:\([^\s()]*\))?))"
    r"(?:\s+(?:\"[^\"]*\"|'[^']*'|\([^)]*\)))?\s*\)")
HTML_LINK = re.compile(r"<(?:a|img)\b[^>]*?\b(href|src)=[\"']([^\"']+)[\"']", re.I)
# A reference definition is the whole line: `[label]: target "title"`.
REF_DEF = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*(?:<([^>]*)>|(\S+))\s*(?:\"[^\"]*\"|'[^']*'|\([^)]*\))?\s*$")
REF_LABEL = re.compile(r"^\s{0,3}\[([^\]]+)\]:\s*\S")
# `[text][label]` and `[label][]`; a bare `[label]` is too often prose ("[code]").
FULL_REF = re.compile(r"(?<![\]\\\w\"])\[([^\[\]\"]+)\]\[([^\[\]\"]*)\]")
CODE_SPAN = re.compile(r"(?<!`)(`+)(?!`)(.+?)(?<!`)\1(?!`)", re.S)
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
HEADING = re.compile(r"^\s{0,3}#{1,6}(?:\s+(.*?))?(?:\s+#+)?\s*$")
SETEXT = re.compile(r"^ {0,3}(?:=+|-+)\s*$")
HTML_ANCHOR = re.compile(r"<a\b[^>]*?\b(?:name|id)=[\"']([^\"']+)[\"']", re.I)
# A Markdown path named in a code span or in code. It must exist only when
# it points at documentation, so a bare name (`SKILL.md`), test data
# (`docs/keep.md`) or a placeholder (`phase-N.md`) is not a reference; with
# an anchor it is checked wherever it resolves.
PATH_MENTION = re.compile(r"(?<![\w./-])((?:[\w.-]+/)*[\w.-]+\.md)(?:#([\w-]+))?")
DOC_PATH = re.compile(r"^(?:docs/(?:adr|agents|book|design|research|security)/|\.claude/(?:agents|skills)/)")
# A top-level doc (`docs/testing.md`) counts only with an anchor: bare, the
# name is as often an example in a doc comment.
TOP_DOC = re.compile(r"^docs/[^/]+\.md$")
PLACEHOLDER = re.compile(r"NNNN|N{2}-|-N\b|X\.Y\.Z")
# Test code holds test data; only these hold paths that look like docs.
TEST_CODE = re.compile(r"^tests/ci/|(^|/)tests\.rs$|^crates/[^/]+/tests/")


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


def marker_files(files):
    """The listed files that may hold markers, with their lines."""
    for rel in files:
        if not is_marker_file(rel):
            continue
        try:
            yield rel, (ROOT / rel).read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError):
            continue


def fetch_issues():
    """One `gh api` call. Returns (issues, skip, error, transient).

    `transient` names a response cut short (gh's "unexpected end of JSON
    input", or a zero exit with output that is not JSON), worth one retry.
    """
    try:
        out = subprocess.run(
            ["gh", "api", "--paginate", f"repos/{REPO}/issues?state=all&per_page=100",
             "--jq", ".[] | [.number, .state, (.pull_request != null), [.labels[].name]] | @json"],
            capture_output=True, text=True, timeout=120, check=False,
        )
    except FileNotFoundError:
        return None, "gh is not installed", None, None
    except subprocess.TimeoutExpired:
        return None, "gh timed out", None, None
    except OSError as e:
        return None, None, f"gh could not run: {e}", None
    if out.returncode != 0:
        err = out.stderr.strip() or f"gh exited with {out.returncode}"
        last = err.splitlines()[-1]
        if NETWORK_ERROR.search(err):
            return None, f"network error: {last}", None, None
        if NOT_LOGGED_IN.search(err):
            return None, "gh is not logged in (run `gh auth login`)", None, None
        if TRUNCATED in err:
            return None, None, None, f"gh: {TRUNCATED}"
        return None, None, f"gh api repos/{REPO}/issues failed: {last}", None
    issues = {}
    for line in out.stdout.splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            return None, None, None, f"gh output is not JSON: {e}"
        try:
            number, state, is_pr, labels = row
        except (ValueError, TypeError) as e:
            return None, None, f"unexpected output from gh: {e}", None
        if not is_pr:
            issues[number] = (state, labels)
    return issues, None, None, None


def github_issues():
    """Asks GitHub for the repository's issues, pull requests left out.

    Returns (issues, skip, error): `issues` maps a number to (state, labels);
    `skip` says why the check cannot run (no `gh`, `gh` not logged in, no
    network, a response cut short twice); `error` is any other failure,
    which is a problem.
    """
    issues, skip, error, transient = fetch_issues()
    if transient:
        time.sleep(RETRY_DELAY)
        issues, skip, error, transient = fetch_issues()
        if transient:
            return None, f"{transient} (twice)", None
    return issues, skip, error


def question_checks(problems, files):
    """Returns why the issue check was skipped, or None."""
    text = QUESTIONS.read_text(encoding="utf-8")
    seen = {}
    for q in QUESTION.findall(text):
        seen[q] = seen.get(q, 0) + 1
    for q, n in sorted(seen.items()):
        if n > 1:
            problems.append(f"docs/open-questions.md: {q} appears {n} times")
    markers = []
    for rel, lines in marker_files(files):
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


def slug(text):
    """GitHub's anchor for a heading's text (mdBook's agrees for these files).

    Code spans keep their text; elsewhere HTML, escapes, link and image markup
    and emphasis are what the reader does not see, so they are dropped first.
    """
    out, pos = [], 0
    for m in CODE_SPAN.finditer(text):
        out.append(_visible(text[pos:m.start()]))
        out.append(m.group(2).strip())
        pos = m.end()
    out.append(_visible(text[pos:]))
    return re.sub(r"[^\w\- ]", "", "".join(out).strip().lower()).replace(" ", "-")


def _visible(text):
    text = re.sub(r"!\[[^\]]*\](?:\([^)]*\)|\[[^\]]*\])?", "", text)
    text = re.sub(r"\[([^\]]*)\](?:\([^)]*\)|\[[^\]]*\])?", r"\1", text)
    text = re.sub(r"<https?://([^>]*)>", r"\1", text)
    text = re.sub(r"</?[A-Za-z][^>]*>", "", text)
    text = re.sub(r"\\(.)", r"\1", html.unescape(text))
    # Emphasis underscores sit at word edges; snake_case ones stay.
    return re.sub(r"(?<![^\W_])_+|_+(?![^\W_])", "", text)


def blocks(lines):
    """Yields (first line number, lines) for each block of prose, outside fenced code."""
    fence, block, start = None, [], 0
    for i, line in enumerate(lines, 1):
        m = FENCE.match(line)
        if fence:
            # Closes only on the opening's character, at least as long.
            if m and m.group(1)[0] == fence[0] and len(m.group(1)) >= len(fence) and not line[m.end():].strip():
                fence = None
            continue
        if m:
            if block:
                yield start, block
            fence, block = m.group(1), []
            continue
        if not line.strip():
            if block:
                yield start, block
            block = []
            continue
        if not block:
            start = i
        block.append(line)
    if block and not fence:
        yield start, block


_anchors = {}


def anchors(path):
    """The anchors a Markdown file defines: headings (numbered like GitHub's duplicates) and `<a name|id>`."""
    if path not in _anchors:
        found, seen = set(), {}
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        front_end = 0
        if lines and lines[0] == "---":
            front_end = next((n for n, l in enumerate(lines[1:], 2) if l == "---"), 0)
        for start, block in blocks(lines):
            for k, line in enumerate(block):
                found.update(HTML_ANCHOR.findall(line))
                if start + k <= front_end:
                    continue
                text = None
                m = HEADING.match(line.lstrip("> "))
                if m:
                    text = m.group(1)
                elif k + 1 < len(block) and SETEXT.match(block[k + 1]) and not HEADING.match(line):
                    text = line
                if text is not None:
                    s = slug(text)
                    n = seen.get(s, 0)
                    found.add(s if n == 0 else f"{s}-{n}")
                    seen[s] = n + 1
        _anchors[path] = found
    return _anchors[path]


def frozen(rel):
    """Records no one rewrites: a broken link there is fixed at its target."""
    return bool(ADR_PATH.match(rel)) or rel.startswith(FROZEN)


class Links:
    """Checks links and named paths against the files `git ls-files` lists."""

    def __init__(self, files, problems):
        self.files = set(files)
        self.dirs = {str(Path(f).parent) for f in files} | {
            str(p) for f in files for p in Path(f).parents if str(p) != "."}
        self.problems = problems

    def exists(self, rel):
        return rel in self.files or rel in self.dirs

    def report(self, rel, i, target, what, frag=None):
        hint = ""
        if frozen(rel):
            hint = (f"; {rel} is frozen: add <a name=\"{frag}\"></a> above the heading it means"
                    if frag else f"; {rel} is frozen: restore the target or ask the maintainer")
        self.problems.append(f"{rel}:{i}: {target}: {what}{hint}")

    def check(self, rel, i, target, mention=False):
        """Checks one link, or with `mention` a path named in code."""
        base = PurePosixPath(rel).parent
        m = REPO_URL.match(target) or REPO_HOME.match(target)
        absolute = bool(m)
        if m:
            path = unquote(m.group("path") or "README.md").rstrip("/")
            frag = m.group("frag")
            if not m.group("path") and frag in (None, "", "readme"):
                return
            candidates = [PurePosixPath(path)]
        elif SCHEME.match(target) or target.startswith("//"):
            return
        else:
            path, _, frag = target.partition("#")
            path = unquote(path.split("?")[0])
            if not path:
                candidates = [PurePosixPath(rel)]
            elif path.startswith("/"):
                candidates = [PurePosixPath(path.lstrip("/"))]
            elif mention:
                candidates = [PurePosixPath(path), base / path]
            else:
                candidates = [base / path]
        resolved = []
        for c in candidates:
            norm = posixpath.normpath(str(c))
            if norm == ".." or norm.startswith("../"):
                if not mention:
                    self.report(rel, i, target, "points outside the repository")
                    return
                continue
            if self.exists(norm):
                resolved.append(norm)
        if not resolved:
            documentation = DOC_PATH.match(path) or (TOP_DOC.match(path) and frag)
            if not mention or (documentation and not PLACEHOLDER.search(path)):
                self.report(rel, i, target, "no such file in the repository")
            return
        dest = resolved[0]
        if rel.startswith(BOOK_SRC) and not (mention or absolute) and not dest.startswith(BOOK_SRC):
            self.report(rel, i, target,
                        "mdBook publishes docs/book/src only; link to "
                        f"https://github.com/{REPO}/blob/main/{dest}")
            return
        frag = unquote(frag or "")
        if not frag or GITHUB_LINE.match(frag):
            return
        md = [d for d in resolved if d.endswith(".md") and d in self.files]
        if md and not any(frag in anchors(ROOT / d) for d in md):
            self.report(rel, i, target, f"{md[0]} has no anchor #{frag}", frag)

    def markdown(self, rel, lines):
        defined = {" ".join(m.group(1).split()).lower() for line in lines if (m := REF_LABEL.match(line))}
        for start, block in blocks(lines):
            text = "\n".join(block)
            offsets = [0]
            for line in block:
                offsets.append(offsets[-1] + len(line) + 1)

            def line_of(pos):
                return start + next(k for k in range(len(block)) if offsets[k + 1] > pos)

            # Code spans may wrap onto the next line, so they are found in
            # the whole block, and blanked out of the prose in place.
            prose = list(text)
            for m in CODE_SPAN.finditer(text):
                for pm in PATH_MENTION.finditer(m.group(2)):
                    self.check(rel, line_of(m.start(2) + pm.start()),
                               pm.group(1) + (f"#{pm.group(2)}" if pm.group(2) else ""), mention=True)
                prose[m.start():m.end()] = re.sub(r"[^\n]", " ", m.group(0))
            prose = "".join(prose)
            for m in INLINE_LINK.finditer(prose):
                target = m.group("angle") if m.group("angle") is not None else m.group("plain")
                if target:
                    self.check(rel, line_of(m.start()), target)
                # An image inside a link's text: `[![alt](img)](target)`.
                for inner in INLINE_LINK.finditer(m.group(0)[1:]):
                    if inner.group("plain"):
                        self.check(rel, line_of(m.start()), inner.group("plain"))
            for m in HTML_LINK.finditer(prose):
                self.check(rel, line_of(m.start()), m.group(2))
            for m in FULL_REF.finditer(prose):
                label = " ".join((m.group(2) or m.group(1)).split()).lower()
                if label not in defined:
                    self.problems.append(f"{rel}:{line_of(m.start())}: [{m.group(1)}][{m.group(2)}]: no definition of [{label}]")
            for k, line in enumerate(prose.split("\n")):
                m = REF_DEF.match(line)
                if m:
                    self.check(rel, start + k, m.group(1) or m.group(2))

    def code(self, rel, lines):
        for i, line in enumerate(lines, 1):
            for m in PATH_MENTION.finditer(line):
                self.check(rel, i, m.group(1) + (f"#{m.group(2)}" if m.group(2) else ""), mention=True)


def link_checks(problems, files):
    links = Links(files, problems)
    for rel in files:
        if rel.startswith(LINK_SKIP):
            continue
        markdown = rel.endswith(".md")
        if not markdown and not (rel.endswith(CODE_SUFFIXES) or rel == "justfile"):
            continue
        if not markdown and TEST_CODE.search(rel):
            continue
        try:
            lines = (ROOT / rel).read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        (links.markdown if markdown else links.code)(rel, lines)


def tracked_files(problems):
    """The repository's files, as `git ls-files` lists them (tracked, or untracked and not ignored)."""
    try:
        out = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            capture_output=True, timeout=60, check=True,
        )
    except (OSError, subprocess.SubprocessError) as e:
        problems.append(f"cannot list the repository's files with git ls-files: {e}")
        return []
    # A file deleted or moved but not yet staged is still listed: drop it.
    return sorted({p for p in out.stdout.decode("utf-8", "replace").split("\0")
                   if p and os.path.lexists(ROOT / p)})


def main():
    problems = []
    files = tracked_files(problems)
    adr_checks(problems)
    link_checks(problems, files)
    skipped = question_checks(problems, files)
    for p in problems:
        print(p)
    if problems:
        print(f"docs-lint: {len(problems)} problem(s)", file=sys.stderr)
        sys.exit(1)
    print(f"docs-lint: ok (issue check skipped: {skipped})" if skipped else "docs-lint: ok")


if __name__ == "__main__":
    main()
