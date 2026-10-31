#!/usr/bin/env python3
"""Print the brief a `host-safety` review needs (`just review-pack <branch>`).

The `host-safety` subagent cannot run Bash, so it cannot diff, query graphify
or find the threat-model rows itself; this collects them for a branch against
`main` (or `--base`):

- the changed files, grouped by crate, and `git diff --stat`;
- `graphify affected` for each changed Rust file, when graphify and a graph
  are available (the graph is the main checkout's: it may lag the branch);
- the threat-model rows (T-ids) and design sections for the touched crates;
- each touched crate's `# Boundaries` section.

Paste the output into the reviewer's prompt with the requirement. Standard
library only; run from the repository root.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path.cwd()

# Threat-model rows and design documents per crate. Keep in step with
# docs/security/threat-model.md and docs/design/.
CRATES = {
    "ranikust": (["T4", "T7", "T11", "T12", "T19"], ["modes-and-detection.md", "kaniko-mode.md", "sandbox.md"]),
    "rnk-rootfs": (["T1", "T2", "T3", "T4", "T7", "T15", "T16", "T18"], ["rootfs.md", "kaniko-mode.md"]),
    "rnk-sandbox": (["T5", "T6", "T7", "T8", "T9", "T10", "T11", "T17", "T18", "T19", "T20"], ["sandbox.md"]),
    "rnk-snapshot": (["T1", "T3", "T4", "T9", "T10", "T13", "T15"], ["overlay-snapshot.md", "kaniko-mode.md"]),
    "rnk-store": (["T4", "T13", "T14"], ["cache-and-store.md"]),
    "rnk-builder": (["T1", "T10", "T11", "T13"], ["overlay-snapshot.md", "kaniko-mode.md"]),
    "rnk-layer": (["T15", "T16"], []),
    "rnk-registry": (["T11", "T13"], ["cache-and-store.md"]),
    "rnk-context": (["T1", "T15", "T16"], []),
    "rnk-dockerfile": (["T16"], []),
    "rnk-ignore": (["T16"], []),
    "rnk-oci": (["T16"], []),
    "rnk-cache": (["T13"], ["cache-and-store.md"]),
    "rnk-sched": (["T12"], ["scheduler.md"]),
    "rnk-metrics": (["T11"], ["metrics.md"]),
}
# Crates whose diff always needs the `host-safety` subagent (CLAUDE.md).
HOST_SAFETY = {"rnk-rootfs", "rnk-sandbox", "rnk-snapshot", "rnk-store"}


def run(*args, check=True):
    out = subprocess.run(args, capture_output=True, text=True)
    if check and out.returncode != 0:
        sys.exit(f"review-pack: {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout


def crate_of(path):
    parts = path.split("/")
    if len(parts) > 1 and parts[0] == "crates":
        return parts[1]
    return "(outside crates)"


def threat_rows(ids):
    text = (ROOT / "docs/security/threat-model.md").read_text(encoding="utf-8")
    rows = {}
    for line in text.splitlines():
        m = re.match(r"^\| (T\d+) \|", line)
        if m and m.group(1) in ids:
            rows[m.group(1)] = line
    return [rows[t] for t in sorted(ids, key=lambda t: int(t[1:])) if t in rows]


def boundaries(crate, branch):
    """The `# Boundaries` section of a crate's root, as on `branch`."""
    for rel in (f"crates/{crate}/src/lib.rs", f"crates/{crate}/src/main.rs"):
        shown = subprocess.run(["git", "show", f"{branch}:{rel}"], capture_output=True, text=True)
        if shown.returncode != 0:
            continue
        lines = shown.stdout.splitlines()
        out, inside = [], False
        for line in lines:
            if not line.startswith("//!"):
                if inside:
                    break
                continue
            body = line[3:].removeprefix(" ")
            if body.startswith("# Boundaries"):
                inside = True
                continue
            if inside:
                if body.startswith("# "):
                    break
                out.append(body)
        if out:
            return rel, "\n".join(out).strip()
    return None, None


def graphify_bin():
    extra = [os.path.expanduser("~/.cargo/bin"), os.path.expanduser("~/.local/bin")]
    path = os.pathsep.join(extra + [os.environ.get("PATH", "")])
    return shutil.which("graphify", path=path)


def main_graph():
    common = Path(run("git", "rev-parse", "--path-format=absolute", "--git-common-dir").strip())
    for candidate in (ROOT / "graphify-out/graph.json", common.parent / "graphify-out/graph.json"):
        if candidate.exists():
            return candidate
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("branch")
    ap.add_argument("--base", default="main")
    ap.add_argument("--depth", default="1", help="graphify affected depth (default 1)")
    a = ap.parse_args()

    merge_base = run("git", "merge-base", a.base, a.branch).strip()
    rng = f"{merge_base}..{a.branch}"
    status = [l.split("\t") for l in run("git", "diff", "--name-status", rng).splitlines() if l]
    files = [cols[-1] for cols in status]
    deleted = {cols[-1] for cols in status if cols[0].startswith("D")}
    if not files:
        sys.exit(f"review-pack: no change between {a.base} and {a.branch}")
    by_crate = {}
    for f in files:
        by_crate.setdefault(crate_of(f), []).append(f)
    crates = sorted(c for c in by_crate if c in CRATES)

    print(f"# Host-safety brief: {a.branch} against {a.base}\n")
    print(f"Commits ({rng}):\n")
    print(run("git", "log", "--oneline", rng).rstrip(), "\n")
    needs = sorted(set(crates) & HOST_SAFETY)
    if needs:
        print(f"Touches {', '.join(needs)}: the `host-safety` subagent is required.\n")
    print("## Changed files\n")
    for crate in sorted(by_crate):
        print(f"- {crate}:")
        for f in by_crate[crate]:
            print(f"  - {f}" + (" (deleted)" if f in deleted else ""))
    print("\n## Diff stat\n\n```")
    print(run("git", "diff", "--stat", rng).rstrip())
    print("```\n")

    gbin, graph = graphify_bin(), main_graph()
    rust = [f for f in files if f.endswith(".rs") and f not in deleted]
    print("## graphify affected (reverse dependencies of the changed Rust files)\n")
    if not (gbin and graph):
        print("graphify or graphify-out/graph.json not found: skipped.\n")
    else:
        print(f"Graph: {graph} (built from the main checkout; it may lag the branch).\n")
        for f in rust:
            out = subprocess.run(
                [gbin, "affected", f, "--graph", str(graph), "--depth", a.depth],
                capture_output=True,
                text=True,
            )
            lines = [l for l in out.stdout.splitlines() if l.startswith("- ")]
            print(f"### {f}\n")
            print("\n".join(lines[:25]) if lines else "(no node in the graph: a new file?)")
            if len(lines) > 25:
                print(f"... {len(lines) - 25} more")
            print()

    ids = sorted({t for c in crates for t in CRATES[c][0]}, key=lambda t: int(t[1:]))
    print("## Threat-model rows (docs/security/threat-model.md)\n")
    for row in threat_rows(set(ids)):
        print(row)
    designs = sorted({d for c in crates for d in CRATES[c][1]})
    print("\n## Design documents to check the change against\n")
    for d in designs:
        print(f"- docs/design/{d}")
    print("- docs/agents/rules.md (Host safety, Security invariants, Code traps)")
    print("\n## Crate boundaries\n")
    for crate in crates:
        rel, text = boundaries(crate, a.branch)
        if text:
            print(f"### {crate} ({rel})\n\n{text}\n")


if __name__ == "__main__":
    main()
