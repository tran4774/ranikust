"""Writes the seed of the VM's e2e suite as a tar stream on stdout.

    python3 -I tests/vm/e2e-seed.py <nextjs-kaniko dir> <images.tar> <images.txt> | <ssh> 'tar -x ...'

Read by tests/vm/vm.sh; unpacked as ~/rnk/e2e/ in the VM. Holds:

- images.tar, images.txt: the reference images (kaniko, BuildKit, registry,
  verdaccio) saved from the host, and the names to tag them with;
- registry/: the repositories of the base and tools images that the
  workloads name by digest (bench/base-image.txt, bench/tools-image.txt),
  copied from the host registry's storage with their blobs, so the digests
  stay the same;
- verdaccio/: the npm proxy's storage, so pnpm installs without npmjs;
- nk/: the three workloads (app, app-cache, app-par) and the files of
  bench/ the suite uses. node_modules, .next, .data and .git are left out.

Nothing in the nextjs-kaniko directory is written.
"""
import os
import sys
import tarfile

WORKLOADS = ("app", "app-cache", "app-par")
SKIP = {"node_modules", ".next", ".data", ".git"}
BENCH = ("bench/cfg-final-opt.env", "bench/base-image.txt", "bench/tools-image.txt",
         "bench/digest-check/check-blobs.py", "verdaccio/config.yaml")


def owned_by_root(info):
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    return info


def add_tree(tar, src, arcname):
    for dirpath, dirnames, filenames in os.walk(src):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP)
        rel = os.path.relpath(dirpath, src)
        base = arcname if rel == "." else f"{arcname}/{rel}"
        tar.add(dirpath, base, recursive=False, filter=owned_by_root)
        for f in sorted(filenames):
            tar.add(os.path.join(dirpath, f), f"{base}/{f}", recursive=False, filter=owned_by_root)


def repo_of(image_file):
    ref = open(image_file).read().strip()  # registry:5000/<repo>@sha256:<hex>
    return ref.split("/", 1)[1].split("@", 1)[0]


def main():
    nk, images_tar, images_txt = sys.argv[1:4]
    v2 = os.path.join(nk, ".data/registry/docker/registry/v2")
    repos = [repo_of(os.path.join(nk, "bench", f)) for f in ("base-image.txt", "tools-image.txt")]
    with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as tar:
        tar.add(images_tar, "images.tar", filter=owned_by_root)
        tar.add(images_txt, "images.txt", filter=owned_by_root)
        blobs = set()
        for repo in repos:
            rdir = os.path.join(v2, "repositories", repo)
            if not os.path.isdir(rdir):
                sys.exit(f"error: {repo} is not in the registry storage under {v2}")
            add_tree(tar, rdir, f"registry/docker/registry/v2/repositories/{repo}")
            for sub in ("_layers/sha256", "_manifests/revisions/sha256"):
                blobs.update(os.listdir(os.path.join(rdir, sub)))
        for h in sorted(blobs):
            rel = f"docker/registry/v2/blobs/sha256/{h[:2]}/{h}/data"
            tar.add(os.path.join(nk, ".data/registry", rel), f"registry/{rel}", filter=owned_by_root)
        verdaccio = os.path.join(nk, ".data/verdaccio")
        if os.path.isdir(verdaccio):
            add_tree(tar, verdaccio, "verdaccio")
        else:
            print("warning: no verdaccio storage; pnpm will fetch from npmjs", file=sys.stderr)
        for w in WORKLOADS:
            add_tree(tar, os.path.join(nk, w), f"nk/{w}")
        for f in BENCH:
            tar.add(os.path.join(nk, f), f"nk/{f}", filter=owned_by_root)


if __name__ == "__main__":
    main()
