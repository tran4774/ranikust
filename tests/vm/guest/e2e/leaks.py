"""Check a flattened ranikust image for what must never reach a layer:
python3 -I leaks.py <listing.tsv.gz> [extra-forbidden-path]...

Fails (exit 1, the offending paths on stdout) when the listing from
flatten.py holds:

- ranikust's own places: `/ranikust` (kaniko mode's fixed ignore entry),
  the default state directory `/var/lib/ranikust`, any `.ranikust-*` name
  (the store's markers and anonymous names);
- the container's runtime markers `/.dockerenv` and `/run/.containerenv`
  (ADR-0052 D1: kept out of layers);
- each extra path given, with everything below it: the harness's own mount
  points and state directories (the build context, the results directory).

Exit 0 and `leaks: none` otherwise.
"""
import gzip
import sys

FIXED = ("/ranikust", "/var/lib/ranikust", "/.dockerenv", "/run/.containerenv")


def below(path, prefix):
    return path == prefix or path.startswith(prefix.rstrip("/") + "/")


def main():
    listing, *extra = sys.argv[1:]
    forbidden = FIXED + tuple(extra)
    bad = []
    with gzip.open(listing, "rt", encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            path = line.split("\t", 1)[0]
            name = path.rsplit("/", 1)[-1]
            if name.startswith(".ranikust-") or any(below(path, p) for p in forbidden):
                bad.append(path)
    if bad:
        print(f"leaks: {len(bad)} paths that must not be in a layer")
        for p in bad[:50]:
            print(f"  {p}")
        sys.exit(1)
    print("leaks: none")


if __name__ == "__main__":
    main()
