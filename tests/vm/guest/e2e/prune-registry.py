"""Delete the blobs no repository of a stopped registry:2 links any more:
python3 -I prune-registry.py <registry root>

The registry never frees a blob when a repository is deleted, and the e2e
suite deletes its `e2e/` namespace before every build: without this, every
build's layers stay on the guest's disk, which then fills (an 80 GiB disk
after a few dozen W1 builds; the VM stopped answering SSH). A blob is kept
when any `_layers` link (layers and configs) or `_manifests/revisions`
link of any repository names it, so the seeded base and tool images stay.
Run it only while the registry is stopped. Prints what it freed.

It runs as root on a directory the registry's container could write, so
it never follows a symbolic link: every directory from the registry root
down to `blobs/sha256` is opened with O_NOFOLLOW, blob directories are
reached through their parent's descriptor and must be named as the
registry names them (two hex digits, then the 64-digit digest starting
with them); anything else there stops the run. It fails closed: an
unreadable directory, no `repositories`, or no link at all while blobs
exist stops it before anything is deleted.
"""
import os
import re
import shutil
import stat
import sys

PREFIX = re.compile(r"[0-9a-f]{2}")
DIGEST = re.compile(r"[0-9a-f]{64}")
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


def die(msg):
    sys.exit(f"prune-registry: {msg}")


def open_dir(name, dir_fd=None):
    try:
        return os.open(name, DIR_FLAGS, dir_fd=dir_fd)
    except OSError as e:
        die(f"cannot open {name!r} as a directory without following links: {e}")


def raise_error(e):
    raise e


def linked_digests(repos_fd):
    """Digests named by a `link` file under repositories (no link followed)."""
    linked = set()
    for dirpath, _dirs, files, _fd in os.fwalk(".", dir_fd=repos_fd, follow_symlinks=False, onerror=raise_error):
        if "link" not in files:
            continue
        parts = dirpath.split(os.sep)
        if "sha256" in parts and ("_layers" in parts or "revisions" in parts) and DIGEST.fullmatch(parts[-1]):
            linked.add(parts[-1])
    return linked


def main():
    root = open_dir(sys.argv[1])
    v2 = root
    for name in ("docker", "registry", "v2"):
        v2 = open_dir(name, dir_fd=v2)
    try:
        repos = open_dir("repositories", dir_fd=v2)
    except SystemExit:
        die("no repositories directory: refusing to treat every blob as unlinked")
    try:
        linked = linked_digests(repos)
    except OSError as e:
        die(f"cannot read every repository: {e}")
    blobs = open_dir("sha256", dir_fd=open_dir("blobs", dir_fd=v2))
    prefixes = []
    with os.scandir(blobs) as it:
        for entry in it:
            if not entry.is_dir(follow_symlinks=False) or not PREFIX.fullmatch(entry.name):
                die(f"unexpected entry blobs/sha256/{entry.name!r}")
            prefixes.append(entry.name)
    plan = []
    for prefix in prefixes:
        pfd = open_dir(prefix, dir_fd=blobs)
        with os.scandir(pfd) as it:
            for entry in it:
                name = entry.name
                if (not entry.is_dir(follow_symlinks=False) or not DIGEST.fullmatch(name)
                        or not name.startswith(prefix)):
                    die(f"unexpected entry blobs/sha256/{prefix}/{name!r}")
                if name not in linked:
                    plan.append((pfd, prefix, name))
    if prefixes and not linked:
        die("blobs exist but no repository links any: refusing to delete them all")
    freed = 0
    for pfd, prefix, name in plan:
        dfd = open_dir(name, dir_fd=pfd)
        try:
            st = os.stat("data", dir_fd=dfd, follow_symlinks=False)
            if stat.S_ISREG(st.st_mode):
                freed += st.st_size
        except FileNotFoundError:
            pass
        os.close(dfd)
        # shutil.rmtree with dir_fd never follows a symbolic link.
        shutil.rmtree(name, dir_fd=pfd)
    print(f"pruned {len(plan)} unlinked blobs, {freed / 1e9:.2f} GB; {len(linked)} linked")


if __name__ == "__main__":
    main()
