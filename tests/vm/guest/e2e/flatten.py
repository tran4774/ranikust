"""Flatten an image from a plain-HTTP registry into a listing of its files.

    python3 -I flatten.py <host:port> <repository> <tag|digest> <listing.tsv.gz> <config.json>

Pulls the manifest (a Docker or OCI manifest, or an index from which linux/amd64
is taken), then streams every layer in order (gzip, zstd through the `zstd`
command, or uncompressed), applies whiteouts and opaque directories, and writes
one line per path of the flattened root, sorted by path:

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
"""
import gzip
import hashlib
import json
import subprocess
import sys
import tarfile
import threading
import urllib.request

MANIFESTS = ", ".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])
INDEXES = ("application/vnd.oci.image.index.v1+json",
           "application/vnd.docker.distribution.manifest.list.v2+json")


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
    """A readable stream of a layer blob's uncompressed bytes."""

    def __init__(self, resp, media_type):
        self.resp = resp
        if "zstd" in media_type:
            self.proc = subprocess.Popen(["zstd", "-dcq"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            self.feeder = threading.Thread(target=self._feed, daemon=True)
            self.feeder.start()
            self.read = self.proc.stdout.read
        elif "gzip" in media_type or media_type.endswith("tar.gzip"):
            self.proc = None
            self.read = gzip.GzipFile(fileobj=resp).read
        else:
            self.proc = None
            self.read = resp.read

    def _feed(self):
        try:
            while chunk := self.resp.read(1 << 20):
                self.proc.stdin.write(chunk)
        finally:
            self.proc.stdin.close()

    def close(self):
        if self.proc:
            self.feeder.join()
            if self.proc.wait() != 0:
                sys.exit("zstd failed to decompress a layer")


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


def apply_layer(root, stream):
    """Applies one layer to `root` (path -> entry list) in place."""
    layer, whiteouts, opaque, links = {}, [], [], []
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
                links.append((path, norm(m.linkname), m))
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
    # Whiteouts and opaque directories hide only what lies below this layer.
    for d in opaque:
        prefix = d.rstrip("/") + "/"
        for p in [p for p in root if p.startswith(prefix)]:
            del root[p]
    for w in whiteouts:
        prefix = w + "/"
        for p in [p for p in root if p == w or p.startswith(prefix)]:
            del root[p]
    for path, entry in layer.items():
        old = root.get(path)
        if old and old[0] == "d" and entry[0] != "d":
            prefix = path + "/"
            for p in [p for p in root if p.startswith(prefix)]:
                del root[p]
        root[path] = entry
    for path, target, m in links:
        t = layer.get(target) or root.get(target)
        if t is None:
            sys.exit(f"hardlink {path} names {target}, which is not in the image")
        t[6] = "h"
        entry = list(t)
        entry[1], entry[2], entry[7] = f"{m.mode & 0o7777:o}", f"{m.uid}:{m.gid}", xattrs(m) if m.pax_headers else t[7]
        root[path] = entry


def main():
    host, repo, ref, listing, config_out = sys.argv[1:6]
    base = f"http://{host}/v2/{repo}"
    m = manifest(base, ref)
    with get(f"{base}/blobs/{m['config']['digest']}") as r:
        config = r.read()
    open(config_out, "wb").write(config)
    root = {}
    for layer in m["layers"]:
        resp = get(f"{base}/blobs/{layer['digest']}")
        stream = Decompressed(resp, layer["mediaType"])
        apply_layer(root, stream)
        stream.close()
        resp.close()
    with gzip.open(listing, "wt", encoding="utf-8", errors="surrogateescape") as out:
        for path in sorted(root):
            out.write(path + "\t" + "\t".join(root[path]) + "\n")
    print(f"{repo}:{ref}: {len(m['layers'])} layers, {len(root)} paths")


if __name__ == "__main__":
    main()
