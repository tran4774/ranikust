"""Compare kaniko's metrics.json with ranikust's report for one build.

    python3 -I metrics.py <kaniko metrics.json> <ranikust metrics.json>

Uses the phase map of docs/design/metrics.md#comparing-with-kaniko: each row
prints kaniko's `total_ns` and ranikust's `total_ns` (and `wall_ns`, the union
of intervals, which differs with parallel work) in seconds. Rows where neither
side has the phase are left out.
"""
import json
import sys

PHASES = [
    ("meta.wall_ns", "meta.wall_ns"),
    ("build.total", "rnk_sched.run"),
    ("push.total", "rnk_builder.output"),
    ("context.fetch", "rnk_context.load"),
    ("base.unpack", "rnk_snapshot.prepare"),
    ("layer.extract", "rnk_rootfs.extract"),
    ("command.exec", "rnk_builder.execute"),
    ("run.child", "rnk_sandbox.run"),
    ("snapshot.command", "rnk_snapshot.diff"),
    ("command.layer", "rnk_builder.layer"),
    ("layer.compress", "rnk_layer.encode"),
    ("stage.finalize", "rnk_builder.assemble"),
    ("fs.delete", "rnk_snapshot.release"),
]


def secs(ns):
    return "-" if ns is None else f"{ns / 1e9:.2f}"


def phase(report, name, key="total_ns"):
    if name == "meta.wall_ns":
        return report.get("meta", {}).get("wall_ns") if key == "total_ns" else None
    p = report.get("phases", {}).get(name)
    return p.get(key) if p else None


def main():
    k, r = (json.load(open(p)) for p in sys.argv[1:3])
    print("| kaniko phase | kaniko total (s) | ranikust span | ranikust total (s) | ranikust wall (s) |")
    print("|---|---|---|---|---|")
    for kp, rp in PHASES:
        kt, rt, rw = phase(k, kp), phase(r, rp), phase(r, rp, "wall_ns")
        if kt is None and rt is None:
            continue
        print(f"| `{kp}` | {secs(kt)} | `{rp}` | {secs(rt)} | {secs(rw)} |")


if __name__ == "__main__":
    main()
