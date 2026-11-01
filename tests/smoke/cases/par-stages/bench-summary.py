"""Summarise the par-stages bench (criterion 7) as Markdown.

    python3 -I bench-summary.py <bench output dir>

Reads bench.tsv (round 0 is the warm-up and is left out) and the
`--metrics-file` reports of round 1: median wall time and peak memory per
configuration, the change against the chained (sequential) build of the same
mode and cap, and, per configuration, each phase's total (summed over the
stages) against its wall time (the union of its intervals), which shows what
ran at once.
"""
import csv
import json
import os
import statistics
import sys

PHASES = [
    "rnk_sched.run",
    "rnk_snapshot.prepare",
    "rnk_sandbox.run",
    "rnk_snapshot.walk",
    "rnk_snapshot.diff",
    "rnk_builder.layer",
    "rnk_layer.encode",
    "rnk_snapshot.release",
]


def main():
    d = sys.argv[1]
    rows = {}
    with open(os.path.join(d, "bench.tsv"), newline="") as f:
        for r in csv.DictReader(f, delimiter="\t"):
            if r["round"] == "0" or r["rc"] != "0":
                continue
            key = (r["mode"], r["cap"], r["work"], r["jobs"])
            peak = int(r["peak_mib"]) if r["peak_mib"].isdecimal() else None
            rows.setdefault(key, []).append((float(r["wall_s"]), peak))

    print("| mode | cap | work | --jobs | wall s, median (min–max) | vs chain | peak MiB, median |")
    print("|---|---|---|---|---|---|---|")
    for key in sorted(rows):
        mode, cap, work, jobs = key
        walls = [w for w, _ in rows[key]]
        peaks = [p for _, p in rows[key] if p is not None]
        med = statistics.median(walls)
        base = rows.get((mode, cap, "chain", "default"))
        rel = "–"
        if base and work != "chain":
            b = statistics.median(w for w, _ in base)
            rel = f"{(med - b) / b * 100:+.0f}%"
        print(f"| {mode} | {cap} | {work} | {jobs} | {med:.2f} ({min(walls):.2f}–{max(walls):.2f}) "
              f"| {rel} | {f'{statistics.median(peaks):.0f}' if peaks else 'unknown'} |")

    print()
    print("Round 1, per phase: total (summed over stages) / wall (union of intervals), seconds.")
    print()
    print("| configuration | build wall | " + " | ".join(f"`{p}`" for p in PHASES[1:]) + " |")
    print("|---|---|" + "---|" * (len(PHASES) - 1))
    for key in sorted(rows):
        mode, cap, work, jobs = key
        path = os.path.join(d, f"{mode}-{cap}-{work}-j{jobs}-r1.json")
        try:
            rep = json.load(open(path))
        except (OSError, ValueError):
            continue
        ph = rep.get("phases", {})

        def cell(name):
            p = ph.get(name)
            if not p:
                return "–"
            return f"{p.get('total_ns', 0) / 1e9:.2f} / {p.get('wall_ns', 0) / 1e9:.2f}"

        wall = rep.get("meta", {}).get("wall_ns")
        wall = f"{wall / 1e9:.2f}" if wall else "–"
        print(f"| {mode} {cap} {work} j{jobs} | {wall} | " + " | ".join(cell(p) for p in PHASES[1:]) + " |")


if __name__ == "__main__":
    main()
