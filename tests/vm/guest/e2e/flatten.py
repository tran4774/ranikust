"""Flatten an image from a plain-HTTP registry into a listing of its files.

    python3 -I flatten.py <host:port> <repository> <tag|digest> <listing.tsv.gz> <config.json>

Pulls the manifest (a Docker or OCI manifest, or an index from which linux/amd64
is taken), then reads every layer (gzip through `pigz` or `gzip`, zstd through
`zstd`, or uncompressed), applies the layers in order with their whiteouts and
opaque directories, and writes one line per path of the flattened root, sorted
by path:

    path  type  mode  uid:gid  size  sha256  link  hardlink  xattrs

- type: f d l c b p (a hardlink is listed as the file it names, with that
  file's size and content);
- mode: octal permission bits with setuid, setgid and sticky;
- link: a symlink's target, or `major,minor` of a device node;
- hardlink: `h` when the entry was written as a hardlink in its layer or is
  the target of one, else `-` (ADR-0012: overlay splits hardlinks);
- xattrs: `name=sha256(value)[:16]` sorted and joined by `,`, else `-`.

The image's config is written as is to <config.json>. These are the fields of
the Phase 1 acceptance criterion (docs/agents/phase-1.md); mtimes are left out
on purpose: kaniko, BuildKit and ranikust differ on them by design.

Each layer is read once into its records (its entries in the order the root
takes them, its whiteouts, opaque directories and hardlinks), kept in the
cache directory RANIKUST_FLATTEN_CACHE under the layer blob's digest and
RECORDS_VERSION: a base layer every builder's image shares is read once per
run. The layers not in the cache are read side by side, by
RANIKUST_FLATTEN_JOBS processes (default: this process's CPUs), each with an
external decompressor. The listing is the same byte for byte as flatten_v1.py's,
the one-pass version it replaces (test_flatten.py checks it).
"""
import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
from concurrent.futures import ProcessPoolExecutor

# Part of a cached record's name: change it whenever what a record holds or
# how it is made changes.
RECORDS_VERSION = "2"

MANIFESTS = ", ".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])
INDEXES = ("application/vnd.oci.image.index.v1+json",
           "application/vnd.docker.distribution.manifest.list.v2+json")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


def get(url, accept=None):
    req = urllib.request.Request(url, headers={"Accept": accept} if accept else {})
    return urllib.request.urlopen(req, timeout=600)


def manifest(base, ref):
    with get(f"{base}/manifests/{ref}", MANIFESTS) as r:
        m = json.load(r)
    if m.get("mediaType") in INDEXES or "manifests" in m:
        for d in m["manifests"]:
            p = d.get("platform", {})
            if p.get("os") == "linux" and p.get("architecture") == "amd64":
                return manifest(base, d["digest"])
        sys.exit(f"no linux/amd64 manifest in {ref}")
    return m


class Decompressed:
    """A readable stream of a layer blob's uncompressed bytes: an external
    decompressor (pigz, else gzip; zstd) fed by a thread, so that
    decompression runs beside the parsing and needs no Python per byte."""

    def __init__(self, resp, media_type):
        self.resp, self.proc = resp, None
        if "zstd" in media_type:
            cmd = ["zstd", "-dcq"]
        elif "gzip" in media_type or media_type.endswith("tar.gzip"):
            cmd = ["pigz", "-dc"] if shutil.which("pigz") else ["gzip", "-dc"]
        else:
            self.read = resp.read
            return
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=1 << 20)
        self.feeder = threading.Thread(target=self._feed, daemon=True)
        self.feeder.start()
        self.read = self.proc.stdout.read

    def _feed(self):
        try:
            while chunk := self.resp.read(1 << 20):
                self.proc.stdin.write(chunk)
        except BrokenPipeError:
            pass
        finally:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass

    def close(self):
        if self.proc:
            # What tar did not read (the archive's zero padding) first, so
            # that neither the decompressor nor the feeder stays blocked.
            while self.proc.stdout.read(1 << 20):
                pass
            self.feeder.join()
            if self.proc.wait() != 0:
                sys.exit(f"{self.proc.args[0]} failed to decompress a layer")


def norm(name):
    name = name.lstrip("/")
    while name.startswith("./"):
        name = name[2:]
    name = name.rstrip("/")
    return "/" + name if name not in ("", ".") else "/"


def parent(path):
    return path.rsplit("/", 1)[0] or "/"


def xattrs(member):
    out = []
    for k, v in sorted(member.pax_headers.items()):
        if k.startswith("SCHILY.xattr."):
            raw = v.encode("utf-8", "surrogateescape")
            out.append(f"{k[len('SCHILY.xattr.'):]}={hashlib.sha256(raw).hexdigest()[:16]}")
    return ",".join(out) or "-"


TYPES = {tarfile.REGTYPE: "f", tarfile.AREGTYPE: "f", tarfile.CONTTYPE: "f", tarfile.DIRTYPE: "d",
         tarfile.SYMTYPE: "l", tarfile.CHRTYPE: "c", tarfile.BLKTYPE: "b", tarfile.FIFOTYPE: "p"}


def read_layer(stream):
    """The records of one layer's tar stream:
    - entries: [path, type, mode, owner, size, sha256, link, "-", xattrs] for
      each path but hardlinks and whiteouts, in the order of the path's first
      appearance, with its last value (what a dict of the layer holds);
    - whiteouts, opaque: the paths they hide below this layer, in order;
    - links: [path, target, mode, owner, xattrs or None] of each hardlink."""
    layer, whiteouts, opaque, links = {}, [], [], []
    # The default buffer: tarfile's stream mode copies what is left of a
    # larger one at every read.
    with tarfile.open(fileobj=stream, mode="r|") as tar:
        for m in tar:
            path = norm(m.name)
            base = path.rsplit("/", 1)[1]
            if base == ".wh..wh..opq":
                opaque.append(parent(path))
                continue
            if base.startswith(".wh."):
                whiteouts.append(parent(path).rstrip("/") + "/" + base[4:])
                continue
            if m.islnk():
                links.append([path, norm(m.linkname), f"{m.mode & 0o7777:o}", f"{m.uid}:{m.gid}",
                              xattrs(m) if m.pax_headers else None])
                continue
            kind = TYPES.get(m.type)
            if kind is None:
                continue
            size, digest, link = "-", "-", "-"
            if kind == "f":
                h = hashlib.sha256()
                f = tar.extractfile(m)
                while chunk := f.read(1 << 20):
                    h.update(chunk)
                size, digest = str(m.size), h.hexdigest()
            elif kind == "l":
                link = m.linkname
            elif kind in "cb":
                link = f"{m.devmajor},{m.devminor}"
            layer[path] = [kind, f"{m.mode & 0o7777:o}", f"{m.uid}:{m.gid}", size, digest, link, "-", xattrs(m)]
    return {"entries": [[p, *e] for p, e in layer.items()], "whiteouts": whiteouts, "opaque": opaque,
            "links": links}


def cache_file(cache, digest):
    return os.path.join(cache, f"{digest.replace(':', '-')}.v{RECORDS_VERSION}.json")


def layer_records(base, layer, cache):
    """A layer's records: from the cache, or read from the registry (and
    put in the cache)."""
    digest = layer["digest"]
    path = cache_file(cache, digest) if cache else None
    if path and os.path.exists(path):
        with open(path, encoding="ascii") as f:
            return json.load(f)
    resp = get(f"{base}/blobs/{digest}")
    stream = Decompressed(resp, layer["mediaType"])
    try:
        rec = read_layer(stream)
    finally:
        stream.close()
        resp.close()
    if path:
        # ensure_ascii: names that are not UTF-8 (surrogates) round-trip.
        fd, tmp = tempfile.mkstemp(dir=cache, prefix=".part-")
        with os.fdopen(fd, "w", encoding="ascii") as f:
            json.dump(rec, f, ensure_ascii=True, separators=(",", ":"))
        os.replace(tmp, path)
    return rec


class Root:
    """The flattened root: path -> entry, as a tree of the paths' components
    (split on "/"), so that hiding everything below a path costs one
    deletion, not a scan of every path (flatten_v1.py scanned all of them
    for each whiteout, which made a layer deleting many files cost minutes).
    "Below" is what flatten_v1.py's string prefixes meant: p below d when p
    starts with d + "/"."""

    def __init__(self):
        self.top = [None, {}]  # [entry, {component: node}]; "/" is top's entry

    @staticmethod
    def parts(path):
        return path[1:].split("/") if path != "/" else []

    def node(self, path, create=False):
        n = self.top
        for c in self.parts(path):
            nxt = n[1].get(c)
            if nxt is None:
                if not create:
                    return None
                nxt = n[1][c] = [None, {}]
            n = nxt
        return n

    def get(self, path):
        n = self.node(path)
        return n[0] if n else None

    def set(self, path, entry):
        self.node(path, create=True)[0] = entry

    def clear_below(self, path):
        """Deletes every p starting with path + "/" (with "/" itself for "/",
        whose prefix "/" every path starts with)."""
        if path == "/":
            self.top = [None, {}]
            return
        n = self.node(path)
        if n:
            n[1] = {}

    def delete(self, path):
        """Deletes path and every p starting with path + "/" ("/" alone for
        "/": no path starts with "//")."""
        if path == "/":
            self.top[0] = None
            return
        *up, last = self.parts(path)
        n = self.top
        for c in up:
            n = n[1].get(c)
            if n is None:
                return
        n[1].pop(last, None)

    def items(self):
        out, stack = [], [("", self.top)]
        while stack:
            prefix, n = stack.pop()
            if n[0] is not None:
                out.append((prefix or "/", n[0]))
            for c, kid in n[1].items():
                stack.append((prefix + "/" + c, kid))
        return out


def apply_records(root, rec):
    """Applies one layer's records to `root` (a Root) in place, as
    flatten_v1.py applied the layer itself."""
    layer = {e[0]: list(e[1:]) for e in rec["entries"]}
    # Whiteouts and opaque directories hide only what lies below this layer.
    for d in rec["opaque"]:
        root.clear_below(d.rstrip("/") or "/")
    for w in rec["whiteouts"]:
        root.delete(w)
    for path, entry in layer.items():
        old = root.get(path)
        # (Below "/" means starting with "//" here, which no path does.)
        if old and old[0] == "d" and entry[0] != "d" and path != "/":
            root.clear_below(path)
        root.set(path, entry)
    for path, target, mode, owner, x in rec["links"]:
        t = layer.get(target) or root.get(target)
        if t is None:
            sys.exit(f"hardlink {path} names {target}, which is not in the image")
        t[6] = "h"
        entry = list(t)
        entry[1], entry[2], entry[7] = mode, owner, x if x is not None else t[7]
        root.set(path, entry)


def jobs():
    n = os.environ.get("RANIKUST_FLATTEN_JOBS", "")
    if n:
        if not n.isdigit() or int(n) < 1:
            sys.exit(f"RANIKUST_FLATTEN_JOBS must be a positive whole number (got {n!r})")
        return int(n)
    return len(os.sched_getaffinity(0))


def main():
    host, repo, ref, listing, config_out = sys.argv[1:6]
    t0 = time.monotonic()
    base = f"http://{host}/v2/{repo}"
    m = manifest(base, ref)
    with get(f"{base}/blobs/{m['config']['digest']}") as r:
        config = r.read()
    open(config_out, "wb").write(config)
    for layer in m["layers"]:
        # The digest names a cache file.
        if not DIGEST.fullmatch(layer["digest"]):
            sys.exit(f"unexpected layer digest {layer['digest']!r}")
    cache = os.environ.get("RANIKUST_FLATTEN_CACHE") or None
    if cache:
        os.makedirs(cache, exist_ok=True)
    todo = [layer for layer in m["layers"] if not (cache and os.path.exists(cache_file(cache, layer["digest"])))]
    records = {}
    n = min(jobs(), len(todo))
    if n > 1:
        with ProcessPoolExecutor(max_workers=n) as pool:
            for layer, rec in zip(todo, pool.map(layer_records, [base] * len(todo), todo, [cache] * len(todo))):
                records[layer["digest"]] = rec
    t_read = time.monotonic() - t0
    root = Root()
    for layer in m["layers"]:
        rec = records.get(layer["digest"]) or layer_records(base, layer, cache)
        apply_records(root, rec)
    entries = sorted(root.items(), key=lambda kv: kv[0])
    with gzip.open(listing, "wt", encoding="utf-8", errors="surrogateescape") as out:
        for path, entry in entries:
            out.write(path + "\t" + "\t".join(entry) + "\n")
    print(f"{repo}:{ref}: {len(m['layers'])} layers, {len(entries)} paths "
          f"({len(m['layers']) - len(todo)} layers from the cache; read {t_read:.1f} s, "
          f"applied and written {time.monotonic() - t0 - t_read:.1f} s)")


if __name__ == "__main__":
    main()
