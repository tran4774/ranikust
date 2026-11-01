"""Delete the blobs no repository of a stopped registry:2 links any more:
python3 -I prune-registry.py <registry root>

The registry never frees a blob when a repository is deleted, and the e2e
suite deletes its `e2e/` namespace before every build: without this, every
build's layers stay on the guest's disk, which then fills (an 80 GiB disk
after a few dozen W1 builds; the VM stopped answering SSH). A blob is kept
when any `_layers` link (layers and configs) or `_manifests/revisions`
link of any repository names it, so the seeded base and tool images stay.
Run it only while the registry is stopped. Prints what it freed.
"""
import os
import shutil
import sys


def main():
    v2 = os.path.join(sys.argv[1], "docker", "registry", "v2")
    linked = set()
    repos = os.path.join(v2, "repositories")
    for dirpath, _dirs, files in os.walk(repos):
        if "link" not in files:
            continue
        parts = dirpath.split(os.sep)
        if "sha256" in parts and ("_layers" in parts or "revisions" in parts):
            linked.add(parts[-1])
    freed = n = 0
    blobs = os.path.join(v2, "blobs", "sha256")
    for prefix in os.listdir(blobs) if os.path.isdir(blobs) else []:
        pdir = os.path.join(blobs, prefix)
        for digest in os.listdir(pdir):
            if digest in linked:
                continue
            ddir = os.path.join(pdir, digest)
            data = os.path.join(ddir, "data")
            freed += os.path.getsize(data) if os.path.exists(data) else 0
            shutil.rmtree(ddir)
            n += 1
    print(f"pruned {n} unlinked blobs, {freed / 1e9:.2f} GB; {len(linked)} linked")


if __name__ == "__main__":
    main()
