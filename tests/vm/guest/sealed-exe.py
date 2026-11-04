"""Checks, as root on the VM host, that ranikust runs from a sealed copy of
its binary (ADR-0101), made the way named, and that no one can write it.

Usage: sealed-exe.py WAY INSTALLED PID...

WAY is overlay or memfd; INSTALLED the binary's path on the host; each PID a
ranikust process or a RUN's init. Every PID must run the same copy, not
the installed file. The copy must be the way named: a read-only overlay
in no mount table, or a memfd with every write and size change sealed.
Even root may not write it through /proc/<pid>/exe: the open is refused,
or (a memfd, on a kernel that let the open through) the seals refuse
rewriting a byte with itself. Nothing is changed in any case.
"""

import ctypes
import errno
import fcntl
import os
import sys

OVERLAYFS_MAGIC = 0x794C7630
SEALS = fcntl.F_SEAL_SEAL | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_GROW | fcntl.F_SEAL_WRITE
LIBC = ctypes.CDLL(None, use_errno=True)


def fail(message):
    print(f"error: sealed-exe: {message}", file=sys.stderr)
    sys.exit(1)


def fs_type(fd):
    # struct statfs starts with f_type, a long on x86_64 and aarch64.
    buf = ctypes.create_string_buffer(256)
    if LIBC.fstatfs(fd, buf) != 0:
        e = ctypes.get_errno()
        raise OSError(e, os.strerror(e))
    return int.from_bytes(buf.raw[:8], sys.byteorder, signed=True) & 0xFFFFFFFF


def mount_ids(pid):
    ids = set()
    for who in ("self", "1", pid):
        with open(f"/proc/{who}/mountinfo") as f:
            ids.update(line.split(" ", 1)[0] for line in f)
    return ids


def mnt_id(fd):
    with open(f"/proc/self/fdinfo/{fd}") as f:
        for line in f:
            if line.startswith("mnt_id:"):
                return line.split(":", 1)[1].strip()
    fail("no mnt_id in fdinfo")


def check_way(way, pid, fd):
    if way == "overlay":
        if fs_type(fd) != OVERLAYFS_MAGIC:
            fail(f"pid {pid}: /proc/{pid}/exe is not on an overlay")
        if not os.fstatvfs(fd).f_flag & os.ST_RDONLY:
            fail(f"pid {pid}: the overlay is not read-only")
        if mnt_id(fd) in mount_ids(pid):
            fail(f"pid {pid}: the overlay is attached somewhere")
    elif way == "memfd":
        seals = fcntl.fcntl(fd, fcntl.F_GET_SEALS)
        if seals & SEALS != SEALS:
            fail(f"pid {pid}: the memfd's seals are {seals:#x}, want {SEALS:#x}")
    else:
        fail(f"unknown way {way}")


def write_refused(way, pid):
    """How each open for writing ended: refused, or its write refused."""
    exe = f"/proc/{pid}/exe"
    want = errno.EROFS if way == "overlay" else errno.ETXTBSY
    with open(exe, "rb") as f:
        first = f.read(1)
    outcomes = []
    for name, flags in (("O_WRONLY", os.O_WRONLY), ("O_RDWR", os.O_RDWR)):
        try:
            fd = os.open(exe, flags | os.O_CLOEXEC)
        except OSError as e:
            if e.errno != want:
                fail(f"pid {pid}: opening {exe} {name}: {e.strerror}, want {os.strerror(want)}")
            outcomes.append(f"{name}: {e.strerror}")
            continue
        try:
            if way != "memfd":
                fail(f"pid {pid}: opened {exe} {name}")
            try:
                os.pwrite(fd, first, 0)
            except OSError as e:
                if e.errno != errno.EPERM:
                    fail(f"pid {pid}: rewriting a byte of {exe}: {e.strerror}, want EPERM")
                outcomes.append(f"{name} opened, its write: {e.strerror}")
            else:
                fail(f"pid {pid}: rewrote a byte of {exe}")
        finally:
            os.close(fd)
    return "; ".join(outcomes)


def main():
    if len(sys.argv) < 4:
        fail("usage: sealed-exe.py WAY INSTALLED PID...")
    way, installed, pids = sys.argv[1], sys.argv[2], sys.argv[3:]
    inst = os.stat(installed)
    copies = set()
    for pid in pids:
        fd = os.open(f"/proc/{pid}/exe", os.O_RDONLY | os.O_CLOEXEC)
        try:
            st = os.fstat(fd)
            if (st.st_dev, st.st_ino) == (inst.st_dev, inst.st_ino):
                fail(f"pid {pid} runs the installed {installed}")
            copies.add((st.st_dev, st.st_ino))
            check_way(way, pid, fd)
        finally:
            os.close(fd)
        why = write_refused(way, pid)
        print(f"pid {pid}: runs a sealed {way} copy ({os.readlink(f'/proc/{pid}/exe')}); "
              f"root cannot write it ({why})")
    if len(copies) != 1:
        fail(f"the processes run {len(copies)} different files, want one copy")


main()
