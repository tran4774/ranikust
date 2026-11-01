"""Pushes the base image of the light e2e workload W0 to the VM's registry.

    python3 -I base-w0.py <host:port> <repository> <busybox>

Prints the image as the builders name it: registry:5000/<repository>@sha256:<hex>.

One gzip layer made here, the same bytes every time (so the same digest): a
static busybox with a link per applet, and what the workload's checks need
from a base: a file with an extended attribute, two names of one inode, a
setuid file, a symlink, a sticky /tmp, and files the workload deletes (a
whiteout of a file and of a directory). Nothing is pulled from a network.
"""
import gzip
import hashlib
import io
import json
import subprocess
import sys
import tarfile
import urllib.request


def member(name, kind=tarfile.REGTYPE, data=b"", mode=0o644, link="", xattrs=None):
    t = tarfile.TarInfo(name)
    t.type, t.mode, t.linkname, t.mtime = kind, mode, link, 0
    t.uid = t.gid = 0
    t.uname = t.gname = "root"
    t.size = len(data) if kind == tarfile.REGTYPE else 0
    if xattrs:
        t.pax_headers = {f"SCHILY.xattr.{k}": v for k, v in xattrs.items()}
    return t, data if kind == tarfile.REGTYPE else None


def layer(busybox):
    exe = open(busybox, "rb").read()
    applets = sorted(set(subprocess.run([busybox, "--list"], stdout=subprocess.PIPE, check=True,
                                        text=True).stdout.split()) - {"busybox"})
    d, f, s, h = tarfile.DIRTYPE, tarfile.REGTYPE, tarfile.SYMTYPE, tarfile.LNKTYPE
    members = [member("bin/", d, mode=0o755), member("bin/busybox", f, exe, mode=0o755)]
    members += [member(f"bin/{a}", s, link="busybox") for a in applets]
    members += [
        member("etc/", d, mode=0o755),
        member("etc/passwd", f, b"root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534::/:/bin/false\n"),
        member("etc/group", f, b"root:x:0:\nnogroup:x:65534:\n"),
        member("etc/remove-me", f, b"the workload deletes this file\n"),
        member("opt/", d, mode=0o755),
        member("opt/xattr", f, b"has an extended attribute\n", xattrs={"user.rnk": "w0"}),
        member("opt/hard-1", f, b"one inode, two names\n"),
        member("opt/hard-2", h, link="opt/hard-1"),
        member("opt/suid", f, b"#!/bin/sh\n", mode=0o4755),
        member("opt/link", s, link="hard-1"),
        member("opt/gone/", d, mode=0o755),
        member("opt/gone/inside", f, b"the workload deletes this directory\n"),
        member("root/", d, mode=0o700),
        member("tmp/", d, mode=0o1777),
    ]
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for t, data in members:
            tar.addfile(t, io.BytesIO(data) if data is not None else None)
    tar_bytes = raw.getvalue()
    return gzip.compress(tar_bytes, mtime=0), "sha256:" + hashlib.sha256(tar_bytes).hexdigest()


def push(base, digest, data):
    loc = urllib.request.urlopen(urllib.request.Request(f"{base}/blobs/uploads/", method="POST"),
                                 timeout=60).headers["Location"]
    if loc.startswith("/"):
        loc = base.split("/v2/")[0] + loc
    sep = "&" if "?" in loc else "?"
    req = urllib.request.Request(f"{loc}{sep}digest={digest}", data=data, method="PUT",
                                 headers={"Content-Type": "application/octet-stream"})
    urllib.request.urlopen(req, timeout=60).close()


def main():
    host, repo, busybox = sys.argv[1:4]
    base = f"http://{host}/v2/{repo}"
    blob, diff_id = layer(busybox)
    layer_digest = "sha256:" + hashlib.sha256(blob).hexdigest()
    config = json.dumps({
        "architecture": "amd64", "os": "linux",
        "config": {"Env": ["PATH=/bin"], "Cmd": ["/bin/sh"]},
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
        "history": [{"created_by": "tests/vm/guest/e2e/base-w0.py"}],
    }, sort_keys=True).encode()
    config_digest = "sha256:" + hashlib.sha256(config).hexdigest()
    push(base, layer_digest, blob)
    push(base, config_digest, config)
    manifest = json.dumps({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest,
                   "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest,
                    "size": len(blob)}],
    }, sort_keys=True).encode()
    req = urllib.request.Request(f"{base}/manifests/w0", data=manifest, method="PUT",
                                 headers={"Content-Type": "application/vnd.oci.image.manifest.v1+json"})
    urllib.request.urlopen(req, timeout=60).close()
    print(f"registry:5000/{repo}@sha256:{hashlib.sha256(manifest).hexdigest()}")


if __name__ == "__main__":
    main()
