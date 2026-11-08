#!/usr/bin/env python3
"""CI lanes and cache housekeeping (ADR-0095). Run with `python3 -I` from the
repository root; standard library and git only.

Subcommands:

  changes [--paths FILE]        print which lanes a run needs, as GitHub
                                outputs (also appended to $GITHUB_OUTPUT):
                                rust, book, t2, deny, full and label_only
  stale-caches NAME...          read `gh cache list --json id,key,ref,createdAt`
                                on stdin and print "<id>\t<key>" for every
                                rust-cache entry of `main` whose cache name is
                                one of NAME and which a newer entry with the
                                same restore key supersedes

`changes` reads the event from the environment, which the workflow fills from
the `github` context (untrusted values reach this script only that way):

  CI_EVENT    github.event_name
  CI_ACTION   github.event.action
  CI_LABEL    github.event.label.name (labeled and unlabeled only)
  CI_DRAFT    github.event.pull_request.draft ("true" or "false")
  CI_LABELS   toJSON(github.event.pull_request.labels.*.name)
  CI_BASE     github.event.pull_request.base.sha
  CI_HEAD     github.event.pull_request.head.sha

On a pull request the changed paths are `git diff --name-only --no-renames
<base>...<head>` (a rename counts as both of its paths); `--paths FILE` (one
path per line, `-` for stdin) gives them instead, for tests and for trying a
list by hand (without CI_EVENT it then stands for a ready pull request). A
push, a manual run (workflow_dispatch) and any other event run every lane.

Lanes, from the paths (RULES below; a path no rule names runs every lane):

  rust   the Rust workspace and what builds or checks it
  book   docs/book/
  t2     rust, or the T2 harness (tests/vm, tests/container, tests/smoke)
  deny   the manifests, Cargo.lock, deny.toml, the toolchain, .cargo/
  full   true unless the pull request is a draft without the `ci:full` label
  label_only
         a label change that changes no lane (any label but `ci:full`, or
         `ci:full` on a pull request that is not a draft): nothing runs

Label names compare case-insensitively, as GitHub's `==` in the workflows'
concurrency expression does. With CI_EVENT set (in a workflow), a missing
$GITHUB_OUTPUT is an error: the gates fail on outputs that are not set.

Tests: tests/ci/test_ci.py (`just test-ci`).
"""

from __future__ import annotations

import datetime
import fnmatch
import json
import os
import re
import subprocess
import sys
import urllib.parse

LANES = ("rust", "book", "t2", "deny")
ALL = frozenset(LANES)
NONE: frozenset[str] = frozenset()
FULL_LABEL = "ci:full"
MAIN_REF = "refs/heads/main"

# A path's lanes are the union of the lanes of every pattern it matches;
# `dir/**` matches everything below dir, and `*` never crosses a `/`. A path
# that matches no pattern runs every lane: when unsure, run the job. The
# tests fail on a tracked file no pattern names, so a new path is classified
# when it is added.
RULES: tuple[tuple[str, frozenset[str]], ...] = (
    # The workflows, this script and what changes every checked-out file.
    (".github/workflows/**", ALL),
    ("scripts/ci.py", ALL),
    (".gitattributes", ALL),
    # The Rust workspace and what builds or checks it.
    ("crates/**", frozenset({"rust"})),
    ("crates/*/Cargo.toml", frozenset({"rust", "deny"})),
    ("Cargo.toml", frozenset({"rust", "deny"})),
    ("Cargo.lock", frozenset({"rust", "deny"})),
    # The toolchain and cargo's configuration also change what cargo-deny
    # resolves and reads.
    ("rust-toolchain.toml", frozenset({"rust", "deny"})),
    (".cargo/**", frozenset({"rust", "deny"})),
    ("rustfmt.toml", frozenset({"rust"})),
    (".rustfmt.toml", frozenset({"rust"})),
    ("clippy.toml", frozenset({"rust"})),
    (".clippy.toml", frozenset({"rust"})),
    # `just test-aarch64` is the native aarch64 job's command.
    ("justfile", frozenset({"rust"})),
    # The static build's check (also the Release workflow's).
    ("scripts/check-static.sh", frozenset({"rust"})),
    ("deny.toml", frozenset({"deny"})),
    ("docs/book/**", frozenset({"book"})),
    # What tests/vm/runner.sh stages and runs on the T2 runners.
    ("tests/vm/**", frozenset({"t2"})),
    ("tests/container/**", frozenset({"t2"})),
    ("tests/smoke/**", frozenset({"t2"})),
    # The AppArmor profile: T2's smoke suite loads it, the book shows it.
    ("contrib/apparmor/**", frozenset({"t2", "book"})),
    # No job of ci.yml or t2.yml reads these. docs-lint runs on every pull
    # request whatever changed, and the `changes` job runs tests/ci itself.
    ("docs/**", NONE),
    ("*.md", NONE),
    (".claude/**", NONE),
    # Named one by one, not `.github/**`: a new directory there (a local
    # action under `.github/actions/`) must fall to "every lane".
    (".github/ISSUE_TEMPLATE/**", NONE),
    (".github/PULL_REQUEST_TEMPLATE.md", NONE),
    (".github/dependabot.yml", NONE),
    (".gitignore", NONE),
    (".graphifyignore", NONE),
    (".editorconfig", NONE),
    ("scripts/docs-lint.py", NONE),
    ("tests/docs/**", NONE),
    ("scripts/review-pack.py", NONE),
    # Local only: the agent worktree clean-up and its test (ADR-0096).
    ("scripts/prune-worktrees.sh", NONE),
    ("tests/agents/**", NONE),
    ("tests/ci/**", NONE),
    # The Release workflow's, which has paths of its own (.github/release.yml
    # groups the release notes).
    (".github/release.yml", NONE),
    ("scripts/release.py", NONE),
    ("scripts/third-party-licenses.hbs", NONE),
    ("tests/release/**", NONE),
    ("licenses/**", NONE),
    ("about.toml", NONE),
    ("cliff.toml", NONE),
    ("LICENSE-APACHE", NONE),
    ("LICENSE-MIT", NONE),
    ("NOTICE", NONE),
    # Local only: benchmarks, the e2e workloads, the test registries, the
    # T1 lane (rootless Podman on a developer's machine).
    ("tests/bench/**", NONE),
    ("tests/e2e/**", NONE),
    ("tests/registry/**", NONE),
    ("tests/rootless/**", NONE),
)

SHA = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
# rust-cache's key (Swatinem/rust-cache v2, src/config.ts): "v0-rust", the
# shared key or the key and the job id, the runner's OS and architecture,
# the environment hash (the restore key ends here) and the files hash.
CACHE_KEY = re.compile(
    r"v0-rust-(?P<name>[A-Za-z0-9_-]+?)-(?P<os>Linux|Darwin|Windows_NT)-(?P<arch>[A-Za-z0-9_]+)"
    r"-(?P<env>[0-9a-f]{8})-(?P<files>[0-9a-f]{8})"
)


def die(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def shown(path: str) -> str:
    """A path as the log shows it: percent-encoded beyond letters, digits,
    `_.-~/`, so no path from a pull request can start or embed a workflow
    command (`::...`, `##[...`) or a line break."""
    return urllib.parse.quote(path.encode("utf-8", "surrogateescape"), safe="/")


def matches(pattern: str, path: str) -> bool:
    if pattern.endswith("/**"):
        return path.startswith(pattern[:-2])
    want, have = pattern.split("/"), path.split("/")
    return len(want) == len(have) and all(fnmatch.fnmatchcase(h, w) for h, w in zip(have, want))


def lanes_of(path: str) -> frozenset[str] | None:
    """The lanes one path needs, or None when no rule names it."""
    found = [lanes for pattern, lanes in RULES if matches(pattern, path)]
    if not found:
        return None
    lanes = frozenset().union(*found)
    return lanes | {"t2"} if "rust" in lanes else lanes


def classify(paths: list[str]) -> tuple[frozenset[str], list[str]]:
    """The lanes a set of changed paths needs, and the paths no rule names."""
    lanes: set[str] = set()
    unknown = []
    for path in paths:
        found = lanes_of(path)
        if found is None:
            unknown.append(path)
            lanes |= ALL
        else:
            lanes |= found
    return frozenset(lanes), unknown


def flag(name: str, value: str) -> bool:
    if value not in ("true", "false"):
        die(f"{name} must be true or false (got {value!r})")
    return value == "true"


def pr_labels(value: str) -> list[str]:
    try:
        labels = json.loads(value)
    except json.JSONDecodeError as e:
        die(f"CI_LABELS is not JSON: {e}")
    if not isinstance(labels, list) or not all(isinstance(x, str) for x in labels):
        die("CI_LABELS must be a JSON list of label names")
    # GitHub's `==` ignores case, so the concurrency group does too.
    return [x.casefold() for x in labels]


def changed_paths(base: str, head: str) -> list[str] | None:
    """The paths changed on head since it left base, or None when git cannot
    tell (then every lane runs)."""
    for name, sha in (("CI_BASE", base), ("CI_HEAD", head)):
        if not SHA.fullmatch(sha):
            die(f"{name} is not a commit hash (got {sha!r})")
    try:
        out = subprocess.run(
            ["git", "diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", f"{base}...{head}", "--"],
            capture_output=True, timeout=120, check=False,
        )
    except (OSError, subprocess.SubprocessError) as e:
        print(f"::warning::git diff could not run ({e}): every lane runs")
        return None
    if out.returncode != 0:
        err = out.stderr.decode("utf-8", "replace").strip().splitlines()
        print(f"::warning::git diff {base}...{head} failed ({shown(err[-1]) if err else out.returncode}): every lane runs")
        return None
    return [p for p in out.stdout.decode("utf-8", "surrogateescape").split("\0") if p]


def read_paths(source: str) -> list[str]:
    try:
        text = sys.stdin.read() if source == "-" else open(source, encoding="utf-8").read()
    except OSError as e:
        die(f"cannot read {source}: {e}")
    return [line for line in text.splitlines() if line]


def emit(outputs: dict[str, bool]) -> None:
    target = os.environ.get("GITHUB_OUTPUT")
    if os.environ.get("CI_EVENT") and not target:
        # In a workflow the outputs must reach the job; the gates also fail
        # on outputs that are not "true" or "false".
        die("CI_EVENT is set but GITHUB_OUTPUT is not: the lanes would reach no job")
    lines = [f"{k}={'true' if v else 'false'}" for k, v in outputs.items()]
    print("\n".join(lines))
    if target:
        with open(target, "a", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")


def summary(rows: list[tuple[str, str]]) -> None:
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if not target:
        return
    with open(target, "a", encoding="utf-8") as f:
        f.write("### Lanes (scripts/ci.py, ADR-0095)\n\n| | |\n|---|---|\n")
        f.writelines(f"| {k} | {v} |\n" for k, v in rows)


def changes(argv: list[str]) -> None:
    source = None
    if argv[:1] == ["--paths"] and len(argv) == 2:
        source = argv[1]
    elif argv:
        die("usage: scripts/ci.py changes [--paths FILE]")
    env = os.environ
    event = env.get("CI_EVENT", "")
    if not event:
        if source is None:
            die("CI_EVENT is not set (the workflow sets it; by hand, use --paths FILE)")
        event, draft, labels, action, label = "pull_request", False, [], "", ""
    elif event == "pull_request":
        draft = flag("CI_DRAFT", env.get("CI_DRAFT", ""))
        labels = pr_labels(env.get("CI_LABELS", ""))
        action, label = env.get("CI_ACTION", ""), env.get("CI_LABEL", "").casefold()
    outputs = {lane: False for lane in LANES} | {"full": False, "label_only": False}

    if event != "pull_request":
        outputs.update({lane: True for lane in LANES}, full=True)
        print(f"{shown(event)}: every lane, full run")
        emit(outputs)
        summary([("event", shown(event)), ("lanes", "all"), ("full", "true")])
        return

    if action in ("labeled", "unlabeled") and not (label == FULL_LABEL and draft):
        # Only `ci:full` on a draft changes what runs; any other label change
        # runs nothing (the workflows give such runs a concurrency group of
        # their own, so they never cancel a run that tests).
        print(f"{action} {shown(label)}: a label change that changes no lane, nothing runs")
        outputs["label_only"] = True
        emit(outputs)
        summary([("event", f"pull_request ({action})"), ("lanes", "none: a label change")])
        return

    full = not draft or FULL_LABEL in labels
    paths = read_paths(source) if source is not None else changed_paths(env.get("CI_BASE", ""), env.get("CI_HEAD", ""))
    if paths is None:
        lanes, unknown = ALL, []
    else:
        lanes, unknown = classify(paths)
        print(f"{len(paths)} changed path(s):")
        for p in paths:
            found = lanes_of(p)
            print(f"changed: {shown(p)} -> {(','.join(sorted(found)) or 'no lane') if found is not None else 'unclassified: every lane'}")
        for p in unknown:
            print(f"::warning::no rule in scripts/ci.py names {shown(p)}: every lane runs")
    outputs.update({lane: lane in lanes for lane in LANES}, full=full)
    print(f"draft: {str(draft).lower()}, {FULL_LABEL}: {str(FULL_LABEL in labels).lower()}")
    emit(outputs)
    summary([
        ("event", f"pull_request ({shown(action) or '?'})"),
        ("draft", str(draft).lower()),
        (FULL_LABEL, str(FULL_LABEL in labels).lower()),
        ("full", str(full).lower()),
        ("changed paths", "unknown (git diff failed): all lanes" if paths is None else str(len(paths))),
        ("unclassified paths", str(len(unknown))),
        ("lanes", ", ".join(lane for lane in LANES if lane in lanes) or "none"),
    ])


def created(entry: dict) -> datetime.datetime:
    try:
        return datetime.datetime.fromisoformat(entry["createdAt"])
    except (TypeError, ValueError) as e:
        die(f"cache {entry['id']}: createdAt is not a timestamp: {e}")


def stale_caches(argv: list[str]) -> None:
    names = set(argv)
    if not names or not all(re.fullmatch(r"[A-Za-z0-9_-]+", n) for n in names):
        die("usage: scripts/ci.py stale-caches NAME... < caches.json")
    try:
        entries = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        die(f"the cache list is not JSON: {e}")
    if not isinstance(entries, list):
        die("the cache list must be a JSON list")
    groups: dict[str, list[dict]] = {}
    for e in entries:
        if not (
            isinstance(e, dict)
            and isinstance(e.get("id"), int)
            and isinstance(e.get("key"), str)
            and isinstance(e.get("ref"), str)
            and isinstance(e.get("createdAt"), str)
        ):
            die("each cache needs an integer id and string key, ref and createdAt")
        # Only main's: a cache of another ref is not this job's to judge.
        if e["ref"] != MAIN_REF:
            continue
        m = CACHE_KEY.fullmatch(e["key"])
        if not m or m["name"] not in names:
            continue
        groups.setdefault(e["key"][: m.start("files") - 1], []).append(e)
    for restore in sorted(groups):
        newest_last = sorted(groups[restore], key=lambda e: (created(e), e["id"]))
        for e in newest_last[:-1]:
            print(f"{e['id']}\t{e['key']}")


def main(argv: list[str]) -> None:
    if not argv:
        die("usage: scripts/ci.py changes [--paths FILE] | stale-caches NAME...")
    cmd, rest = argv[0], argv[1:]
    if cmd == "changes":
        changes(rest)
    elif cmd == "stale-caches":
        stale_caches(rest)
    else:
        die(f"unknown subcommand {cmd!r}")


if __name__ == "__main__":
    main(sys.argv[1:])
