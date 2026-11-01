"""Check a flattened ranikust image for what must never reach a layer:
python3 -I leaks.py [--reference <listing.tsv.gz>] <listing.tsv.gz> [extra-forbidden-path]...

Fails (exit 1, the offending paths on stdout) when the listing from
flatten.py holds:

- ranikust's own places: `/ranikust` (kaniko mode's fixed ignore entry),
  the default state directory `/var/lib/ranikust`, any `.ranikust-*` name
  (the store's markers and anonymous names);
- the container's runtime markers `/.dockerenv` and `/run/.containerenv`
  (ADR-0052 D1: kept out of layers);
- each extra path given, with everything below it: the harness's own mount
  points and state directories (the build context, the results directory);
- with --reference (kaniko's listing of the same workload), any of the
  files a runtime mounts or a harness binds into the build container
  (`/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname`, anything under
  `/etc/ssl/certs`) that kaniko's image does not have, or has with other
  content: the listing is flattened, so the base image's own copies are
  told apart by kaniko's image, which keeps mount points out of layers.

Exit 0 and `leaks: none` otherwise.
"""
import gzip
import sys

FIXED = ("/ranikust", "/var/lib/ranikust", "/.dockerenv", "/run/.containerenv")
MOUNTED = ("/etc/hosts", "/etc/resolv.conf", "/etc/hostname", "/etc/ssl/certs")


def below(path, prefix):
    return path == prefix or path.startswith(prefix.rstrip("/") + "/")


def load(listing):
    out = {}
    with gzip.open(listing, "rt", encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            path, *rest = line.rstrip("\n").split("\t")
            out[path] = rest
    return out


def main():
    args = sys.argv[1:]
    reference = None
    if args[:1] == ["--reference"]:
        reference = load(args[1])
        args = args[2:]
    listing, *extra = args
    forbidden = FIXED + tuple(extra)
    entries = load(listing)
    bad = []
    for path, rest in entries.items():
        name = path.rsplit("/", 1)[-1]
        if name.startswith(".ranikust-") or any(below(path, p) for p in forbidden):
            bad.append(path)
        elif reference is not None and any(below(path, p) for p in MOUNTED):
            # type, mode, owner, size, sha256, link must match kaniko's.
            ref = reference.get(path)
            if ref is None or ref[:6] != rest[:6]:
                bad.append(path)
    if bad:
        print(f"leaks: {len(bad)} paths that must not be in a layer")
        for p in sorted(bad)[:50]:
            print(f"  {p}")
        sys.exit(1)
    print("leaks: none" + ("" if reference is not None else " (no reference: mounted files not checked)"))


if __name__ == "__main__":
    main()
