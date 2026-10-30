"""Compare two flattened images:
python3 -I compare.py [--noise PATH]... <name-a> <dir-a> <name-b> <dir-b>

Each directory holds `listing.tsv.gz` and `config.json` from flatten.py. Prints
the paths only one side has, the paths whose metadata differs and those whose
content alone differs, each grouped by directory, then the config fields that
differ. Differences fall in three classes:

- noise: files that are not deterministic between two runs of the same
  builder on W1 (experiment 07, section "Image check": the pnpm store,
  node's compile cache, npm's cache, two pnpm state files, /etc/hostname),
  plus each path given with --noise (W3's `du`-derived files, Q-187);
- hardlink: an entry that is a hardlink on either side (ADR-0012: overlay
  splits hardlinks on copy-up, kaniko rewrites every name);
- other: everything else, which the Phase 1 acceptance criterion requires to
  be empty between ranikust and kaniko/BuildKit.

The last line is machine-readable: `result: other=<n> hardlink=<n> noise=<n> config=<n>`.
"""
import collections
import gzip
import json
import sys

NOISE_PREFIXES = ("/root/.local/share/", "/tmp/node-compile-cache/", "/root/.npm/_cacache/", "/root/.cache/")
NOISE_NAMES = (".modules.yaml", ".pnpm-workspace-state-v1.json")
NOISE_PATHS = ("/etc/hostname",)
CONFIG_FIELDS = ("Env", "Cmd", "Entrypoint", "WorkingDir", "User", "Labels", "ExposedPorts", "Volumes",
                 "StopSignal", "Shell", "Healthcheck", "OnBuild")
FIELDS = ("type", "mode", "owner", "size", "sha256", "link", "hardlink", "xattrs")


def load(d):
    entries = {}
    with gzip.open(f"{d}/listing.tsv.gz", "rt", encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            path, *rest = line.rstrip("\n").split("\t")
            entries[path] = rest
    return entries, json.load(open(f"{d}/config.json"))


def noise(path, extra=()):
    return (path.startswith(NOISE_PREFIXES) or path in NOISE_PATHS or path in extra
            or path.rsplit("/", 1)[-1] in NOISE_NAMES)


def group(paths, n=5):
    c = collections.Counter("/".join(p.split("/")[:4]) for p in paths)
    return ", ".join(f"{k} ({v})" for k, v in c.most_common(n)) or "-"


def main():
    args, extra = sys.argv[1:], []
    while args[:1] == ["--noise"]:
        extra.append(args[1])
        args = args[2:]
    if len(args) != 4:
        sys.exit(__doc__)
    na, da, nb, db = args
    a, ca = load(da)
    b, cb = load(db)
    classes = {"other": [], "hardlink": [], "noise": []}

    def classify(kind, path, ea, eb):
        if noise(path, extra):
            cls = "noise"
        elif (ea and ea[6] == "h") or (eb and eb[6] == "h"):
            cls = "hardlink"
        else:
            cls = "other"
        classes[cls].append((kind, path, ea, eb))

    for p in a.keys() - b.keys():
        classify(f"only {na}", p, a[p], None)
    for p in b.keys() - a.keys():
        classify(f"only {nb}", p, None, b[p])
    for p in a.keys() & b.keys():
        ea, eb = a[p], b[p]
        meta = [FIELDS[i] for i in range(len(FIELDS)) if i not in (4, 6) and ea[i] != eb[i]]
        if meta:
            classify("metadata (" + ",".join(meta) + ")", p, ea, eb)
        elif ea[4] != eb[4]:
            classify("content", p, ea, eb)

    print(f"{na}: {len(a)} paths, {nb}: {len(b)} paths")
    for cls in ("other", "hardlink", "noise"):
        items = classes[cls]
        print(f"\n## {cls}: {len(items)}")
        kinds = collections.defaultdict(list)
        for kind, path, ea, eb in items:
            kinds[kind.split(" (")[0] if kind.startswith("metadata") else kind].append((kind, path, ea, eb))
        for kind, rows in sorted(kinds.items()):
            print(f"- {kind}: {len(rows)}: {group([r[1] for r in rows])}")
            if cls == "other":
                for k, path, ea, eb in sorted(rows, key=lambda r: r[1])[:10]:
                    print(f"    {path}  {k}\n      {na}: {ea}\n      {nb}: {eb}")

    cfa, cfb = ca.get("config") or {}, cb.get("config") or {}
    diffs = [f for f in CONFIG_FIELDS if cfa.get(f) != cfb.get(f)]
    for f in ("os", "architecture"):
        if ca.get(f) != cb.get(f):
            diffs.append(f)
    la, lb = len(ca.get("rootfs", {}).get("diff_ids", [])), len(cb.get("rootfs", {}).get("diff_ids", []))
    ha = [h.get("created_by", "") for h in ca.get("history", [])]
    hb = [h.get("created_by", "") for h in cb.get("history", [])]
    print(f"\n## config: {len(diffs)} fields differ; layers {na} {la}, {nb} {lb}; history {na} {len(ha)}, {nb} {len(hb)}")
    for f in diffs:
        print(f"- {f}\n    {na}: {json.dumps(cfa.get(f, ca.get(f)))}\n    {nb}: {json.dumps(cfb.get(f, cb.get(f)))}")
    print(f"\nresult: other={len(classes['other'])} hardlink={len(classes['hardlink'])} "
          f"noise={len(classes['noise'])} config={len(diffs)}")


if __name__ == "__main__":
    main()
