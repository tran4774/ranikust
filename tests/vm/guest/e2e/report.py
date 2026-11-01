"""Summarise an e2e run: python3 -I report.py <out/e2e>

Reads every <workload>/<builder>-r<round>/run.json and the compare-*.txt and
metrics-*.md files next to them; prints a Markdown report: host wall time per
workload and builder (median, min-max over the rounds), the status of each
ranikust build, the image checks and the metrics comparison.
"""
import json
import os
import statistics
import sys


def main():
    root = sys.argv[1]
    runs = []
    for w in sorted(os.listdir(root)):
        wd = os.path.join(root, w)
        if not os.path.isdir(wd) or w == "warmup":
            continue
        for b in sorted(os.listdir(wd)):
            f = os.path.join(wd, b, "run.json")
            if os.path.exists(f):
                runs.append(json.load(open(f)))
    print("# e2e results\n")
    env = os.path.join(root, "env.txt")
    if os.path.exists(env):
        print("```\n" + open(env).read().strip() + "\n```\n")
    print("## Host wall time (s)\n")
    print("| Workload | Builder | Rounds | Median | Min-max | Status |")
    print("|---|---|---|---|---|---|")
    keys = sorted({(r["workload"], r["builder"]) for r in runs})
    for w, b in keys:
        rs = [r for r in runs if r["workload"] == w and r["builder"] == b]
        ok = [r["wall_secs"] for r in rs if r["status"] == "ok"]
        status = ",".join(sorted({r["status"] for r in rs}))
        med = f"{statistics.median(ok):.2f}" if ok else "-"
        rng = f"{min(ok):.2f}-{max(ok):.2f}" if ok else "-"
        print(f"| {w} | {b} | {len(rs)} | {med} | {rng} | {status} |")
    pending = [r for r in runs if r["status"] == "pending"]
    if pending:
        print("\n## Pending builds\n")
        for r in pending[:6]:
            print(f"- {r['workload']} {r['builder']}: {r.get('reason', '')}")
    for w in sorted(os.listdir(root)):
        wd = os.path.join(root, w)
        if not os.path.isdir(wd) or w == "warmup":
            continue
        files = sorted(f for f in os.listdir(wd) if f.startswith(("compare-", "metrics-", "digests")))
        if not files:
            continue
        print(f"\n## {w}\n")
        for f in files:
            text = open(os.path.join(wd, f)).read().strip()
            if not text:
                continue
            if f.startswith("compare-"):
                last = text.splitlines()[-1] if text else ""
                head = text.splitlines()[0] if text else ""
                print(f"- `{f}`: {head}; {last}")
            else:
                print(f"### {f}\n\n{text}\n")
    phases(root)


def phases(root):
    """Where the suite's time went: phases.tsv (one row per build and step:
    wipe, the timed build, flatten, leaks, blobs; compare once) and
    suite-seconds.txt."""
    f = os.path.join(root, "phases.tsv")
    if not os.path.exists(f):
        return
    steps = {}
    for line in open(f).read().splitlines()[1:]:
        w, build, step, secs = line.split("\t")
        steps.setdefault(step, []).append(float(secs))
    print("\n## Time per step (s)\n")
    print("| Step | Count | Total | Max |")
    print("|---|---|---|---|")
    for step in ("wipe", "build", "flatten", "leaks", "blobs", "compare"):
        if step in steps:
            v = steps.pop(step)
            print(f"| {step} | {len(v)} | {sum(v):.1f} | {max(v):.1f} |")
    for step, v in sorted(steps.items()):
        print(f"| {step} | {len(v)} | {sum(v):.1f} | {max(v):.1f} |")
    wall = os.path.join(root, "suite-seconds.txt")
    if os.path.exists(wall):
        print(f"\nSuite wall time (images loaded to report): {float(open(wall).read()):.1f} s")


if __name__ == "__main__":
    main()
