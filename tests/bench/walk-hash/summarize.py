"""Medians (min-max) per configuration from run.sh's JSON lines."""
import json, statistics, sys, collections, pathlib
out = pathlib.Path(sys.argv[1])
runs = collections.defaultdict(list)
for line in (out / "walk.jsonl").read_text().splitlines():
    r = json.loads(line)
    runs[(r["walker"], r["threads"], r["hash"])].append(r["secs"])
hashes = ["none", "xxh3", "highway", "sha256"]
lines = ["| walker | threads | " + " | ".join(hashes) + " |", "|---|---:|" + "---:|" * len(hashes)]
for walker in ["fd", "jwalk"]:
    for threads in sorted({k[1] for k in runs}):
        cells = []
        for h in hashes:
            v = runs[(walker, threads, h)]
            cells.append(f"{statistics.median(v):.3f} ({min(v):.3f}–{max(v):.3f})")
        lines.append(f"| {walker} | {threads} | " + " | ".join(cells) + " |")
lines += ["", "| hash | threads | GiB/s (median of 5) |", "|---|---:|---:|"]
for line in (out / "mem.jsonl").read_text().splitlines():
    r = json.loads(line)
    lines.append(f"| {r['hash']} | {r['threads']} | {r['gib_per_s']:.2f} |")
text = "\n".join(lines) + "\n"
(out / "summary.md").write_text(text)
print(text)
