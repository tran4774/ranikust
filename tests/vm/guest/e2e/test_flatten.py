"""flatten.py writes the same listing as flatten_v1.py, the one-pass version it
replaced, byte for byte: from the network, from its cache, with one process or
several, with pigz or gzip.

    python3 -I -m unittest discover -s tests/vm/guest/e2e -p 'test_*.py'

A small image is served by a registry in this process: three layers (gzip,
zstd when the `zstd` command exists, uncompressed) with whiteouts, an opaque
directory, hardlinks within a layer and to a lower one (with xattrs of its
own), a directory replaced by a file, a path written twice in one layer,
device nodes, a FIFO, setuid and sticky modes, xattrs, a name that is not
UTF-8 and `./` prefixes.
"""
import gzip
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))


def tar_layer(members):
    """members: (TarInfo, bytes or None) in order -> the tar's bytes."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT, encoding="utf-8",
                      errors="surrogateescape") as tar:
        for info, data in members:
            tar.addfile(info, io.BytesIO(data) if data is not None else None)
    return buf.getvalue()


def info(name, kind=tarfile.REGTYPE, data=None, mode=0o644, uid=0, gid=0, link="", xattrs=None, dev=(0, 0)):
    t = tarfile.TarInfo(name)
    t.type, t.mode, t.uid, t.gid, t.linkname = kind, mode, uid, gid, link
    t.size = len(data) if data is not None else 0
    t.devmajor, t.devminor = dev
    if xattrs:
        t.pax_headers = {f"SCHILY.xattr.{k}": v for k, v in xattrs.items()}
    return t, data


def image_layers():
    d, f, s, h, c, b, p = (tarfile.DIRTYPE, tarfile.REGTYPE, tarfile.SYMTYPE, tarfile.LNKTYPE,
                           tarfile.CHRTYPE, tarfile.BLKTYPE, tarfile.FIFOTYPE)
    one = [
        info("etc/", d, mode=0o755), info("etc/passwd", f, b"root:x:0:0\n"),
        info("./a/", d, mode=0o755), info("./a/b/", d, mode=0o700, uid=1000, gid=1000),
        info("a/b/f1", f, b"one" * 1000, mode=0o4755, xattrs={"user.k": "v", "security.capability": "\x01\x02"}),
        info("a/x", f, b"x"), info("lnk", s, link="a/x"), info("a/hl", h, link="a/b/f1"),
        info("keep", f, b"keep", mode=0o600, uid=33, gid=33),
        info("dev/null", c, mode=0o666, dev=(1, 3)), info("dev/sda", b, mode=0o660, dev=(8, 0)),
        info("run/fifo", p, mode=0o600), info("tmp/", d, mode=0o1777),
        info("caf\udce9", f, b"latin-1 name"), info("dup", f, b"first"), info("dup", f, b"second", mode=0o700),
    ]
    two = [
        info("a/.wh.x", f, b""), info("etc/.wh..wh..opq", f, b""), info("etc/new", f, b"new"),
        info("a/b", f, b"was a directory"), info("a/hl2", h, link="keep", mode=0o640, xattrs={"user.l": "w"}),
        info("a/hl3", h, link="keep"), info("dup", f, b"third"),
    ]
    three = [info(".wh.a", f, b""), info("a/", d, mode=0o750), info("a/z", f, b"z"), info("lnk", f, b"no longer a link")]
    return [one, two, three]


def edge_layers():
    """Names flatten_v1.py took as strings: an inner "//", an empty whiteout
    name (".wh." hides "<dir>/" and what starts with "<dir>//"), the root
    directory itself, nested opaque directories, and an opaque directory at
    the top (v1's prefix "/" hides everything, "/" included)."""
    d, f = tarfile.DIRTYPE, tarfile.REGTYPE
    one = [info("./", d, mode=0o755), info("a//b", f, b"double"), info("a/c", f, b"c"), info("top", f, b"t"),
           info("d/e/f", f, b"f"), info("d/e/g/h", f, b"h"), info("d/x", f, b"x")]
    two = [info("a/.wh.", f, b""), info("d/e/.wh..wh..opq", f, b""), info("d/e/new", f, b"n"),
           info("d/.wh.x", f, b"")]
    three = [info(".wh..wh..opq", f, b""), info("fresh", f, b"fresh")]
    return [one, two, three]


def compress(raw, kind):
    if kind == "gzip":
        return gzip.compress(raw, mtime=0), "application/vnd.oci.image.layer.v1.tar+gzip"
    if kind == "zstd":
        out = subprocess.run(["zstd", "-q", "-c"], input=raw, stdout=subprocess.PIPE, check=True).stdout
        return out, "application/vnd.oci.image.layer.v1.tar+zstd"
    return raw, "application/vnd.oci.image.layer.v1.tar"


class Registry:
    """A read-only registry of one repository, in memory."""

    def __init__(self):
        self.blobs, self.manifests = {}, {}

    def add_blob(self, data):
        digest = "sha256:" + hashlib.sha256(data).hexdigest()
        self.blobs[digest] = data
        return digest

    def serve(self):
        reg = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                parts = self.path.split("/")
                body, ctype = None, "application/octet-stream"
                if len(parts) == 5 and parts[3] == "manifests":
                    body, ctype = reg.manifests.get(parts[4]), "application/vnd.oci.image.manifest.v1+json"
                elif len(parts) == 5 and parts[3] == "blobs":
                    body = reg.blobs.get(parts[4])
                if body is None:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return f"127.0.0.1:{self.server.server_address[1]}"


class FlattenTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.reg = Registry()
        kinds = ["gzip", "zstd" if shutil.which("zstd") else "gzip", "none"]
        config = cls.reg.add_blob(json.dumps({"config": {"Cmd": ["sh"]}}).encode())

        def image(tag, all_layers):
            layers = []
            for members, kind in zip(all_layers, kinds):
                data, media = compress(tar_layer(members), kind)
                layers.append({"mediaType": media, "digest": cls.reg.add_blob(data), "size": len(data)})
            cls.reg.manifests[tag] = json.dumps({
                "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config},
                "layers": layers}).encode()

        image("latest", image_layers())
        # A blob served under a digest that is not its own.
        good = tar_layer(image_layers()[0])
        wrong = "sha256:" + hashlib.sha256(b"not the blob").hexdigest()
        cls.reg.blobs[wrong] = good
        cls.reg.manifests["wrong-digest"] = json.dumps({
            "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": wrong,
                        "size": len(good)}]}).encode()
        edge = edge_layers()
        image("edge", edge[:2])
        image("edge-wipe", edge)
        cls.host = cls.reg.serve()
        cls.tmp = tempfile.mkdtemp()

    @classmethod
    def tearDownClass(cls):
        cls.reg.server.shutdown()
        shutil.rmtree(cls.tmp)

    def flatten(self, script, name, env=None, tag="latest"):
        listing = os.path.join(self.tmp, f"{name}.tsv.gz")
        # Only what this run sets: no cache or job count from the caller's
        # environment.
        base = {k: v for k, v in os.environ.items() if not k.startswith("RANIKUST_FLATTEN_")}
        subprocess.run([sys.executable, "-I", os.path.join(HERE, script), self.host, "img", tag, listing,
                        os.path.join(self.tmp, f"{name}.json")], check=True, stdout=subprocess.DEVNULL,
                       env={**base, **(env or {})})
        with gzip.open(listing, "rb") as f:
            return f.read()

    def test_string_prefix_edges_are_the_same_as_v1s(self):
        for tag in ("edge", "edge-wipe"):
            with self.subTest(tag):
                want = self.flatten("flatten_v1.py", f"{tag}-v1", tag=tag)
                self.assertEqual(self.flatten("flatten.py", f"{tag}-v2", {"RANIKUST_FLATTEN_JOBS": "2"}, tag), want)
        # The wipe leaves only what the last layer adds.
        self.assertEqual([l.split("\t")[0] for l in want.decode().splitlines()], ["/fresh"])

    def test_the_listing_is_the_same_as_v1s(self):
        want = self.flatten("flatten_v1.py", "v1")
        lines = want.decode("utf-8", "surrogateescape").splitlines()
        # The image exercises what it claims to.
        self.assertTrue(any("\th\t" in l for l in lines), "no hardlink in the listing")
        self.assertFalse(any(l.startswith(("/a/x\t", "/a/b/f1\t", "/etc/passwd\t", "/a/hl\t")) for l in lines))
        self.assertTrue(any(l.startswith("/caf\udce9\t") for l in lines))
        cache = os.path.join(self.tmp, "cache")
        runs = {
            "one process, no cache": {"RANIKUST_FLATTEN_JOBS": "1"},
            "four processes, no cache": {"RANIKUST_FLATTEN_JOBS": "4"},
            "four processes, filling the cache": {"RANIKUST_FLATTEN_JOBS": "4", "RANIKUST_FLATTEN_CACHE": cache},
            "from the cache": {"RANIKUST_FLATTEN_JOBS": "4", "RANIKUST_FLATTEN_CACHE": cache},
        }
        for i, (what, env) in enumerate(runs.items()):
            with self.subTest(what):
                self.assertEqual(self.flatten("flatten.py", f"v2-{i}", env), want)
        self.assertEqual(len([n for n in os.listdir(cache) if n.endswith(".json")]), 3)

    def test_a_blob_that_is_not_its_digest_is_refused(self):
        cache = os.path.join(self.tmp, "cache-wrong")
        with self.assertRaises(subprocess.CalledProcessError):
            self.flatten("flatten.py", "wrong", {"RANIKUST_FLATTEN_CACHE": cache}, tag="wrong-digest")
        # Nothing was cached from it.
        self.assertEqual([n for n in os.listdir(cache) if n.endswith(".json")] if os.path.isdir(cache) else [], [])

    def test_gzip_without_pigz(self):
        want = self.flatten("flatten_v1.py", "v1-np")
        # A PATH with gzip and zstd but no pigz.
        bin_dir = os.path.join(self.tmp, "bin")
        os.makedirs(bin_dir, exist_ok=True)
        for tool in ("gzip", "zstd"):
            if shutil.which(tool):
                os.symlink(shutil.which(tool), os.path.join(bin_dir, tool))
        self.assertEqual(self.flatten("flatten.py", "v2-np", {"PATH": bin_dir, "RANIKUST_FLATTEN_JOBS": "2"}), want)


if __name__ == "__main__":
    unittest.main()
