import ctypes, os, sys

# Records what case.sh checks, one line per probe. A refused step is the
# probe's result; in_child turns an exception into a line, so the RUN
# succeeds wherever it runs.
report = sys.argv[1]
libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER = 0x10000000
CLONE_NEWNS = 0x00020000
CLONE_NEWCGROUP = 0x02000000


def failed(step):
    return "%s:%s" % (step, os.strerror(ctypes.get_errno()))


def rd(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError as e:
        return os.strerror(e.errno)


def map_root():
    """Maps this process's ids to root in a new user namespace; returns the
    step that failed, or None."""
    uid, gid = os.getuid(), os.getgid()
    if libc.unshare(CLONE_NEWUSER) != 0:
        return failed("unshare")
    for name, text in (("setgroups", "deny"), ("uid_map", "0 %d 1" % uid), ("gid_map", "0 %d 1" % gid)):
        try:
            with open("/proc/self/" + name, "w") as f:
                f.write(text)
        except OSError as e:
            return "%s:%s" % (name, os.strerror(e.errno))
    return None


def in_child(probe):
    """Runs `probe` in a child process and returns the line it recorded."""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(r)
            try:
                line = probe()
            except Exception as e:
                line = "raised:%r" % (e,)
            os.write(w, line.encode())
        finally:
            os._exit(0)
    os.close(w)
    out = b""
    while True:
        chunk = os.read(r, 4096)
        if not chunk:
            break
        out += chunk
    os.close(r)
    _, status = os.waitpid(pid, 0)
    return out.decode() or "no-result:status %d" % status


def setns_init():
    try:
        fd = os.open("/proc/1/ns/cgroup", os.O_RDONLY)
    except OSError as e:
        return "open:" + os.strerror(e.errno)
    r = libc.setns(fd, CLONE_NEWCGROUP)
    e = ctypes.get_errno()
    os.close(fd)
    return "joined" if r == 0 else "setns:" + os.strerror(e)


def setns_from_userns():
    return map_root() or setns_init()


def mounted():
    step = map_root()
    if step:
        return "refused " + step
    if libc.unshare(CLONE_NEWNS | CLONE_NEWCGROUP) != 0:
        return "refused " + failed("unshare")
    os.makedirs("/tmp/cg", exist_ok=True)
    if libc.mount(b"none", b"/tmp/cg", b"cgroup2", 0, None) != 0:
        return "refused " + failed("mount")
    kids = sorted(e for e in os.listdir("/tmp/cg") if os.path.isdir("/tmp/cg/" + e))
    return "me=%d procs=%s children=%s" % (os.getpid(), ",".join(rd("/tmp/cg/cgroup.procs").split()), ",".join(kids))


def cgroups():
    """Every line of the /proc/<pid>/cgroup of each process the RUN can see,
    the init included; one that ended meanwhile is left out."""
    seen = set()
    for p in os.listdir("/proc"):
        if p.isdigit():
            try:
                with open("/proc/%s/cgroup" % p) as f:
                    seen.update(f.read().split())
            except (FileNotFoundError, ProcessLookupError):
                pass
    return "|".join(sorted(seen))


lines = [
    "cgroups=" + cgroups(),
    "setns-plain=" + setns_init(),
    "setns-userns=" + in_child(setns_from_userns),
    "mount-ns=" + in_child(mounted),
]
with open(report, "w") as f:
    f.write("\n".join(lines) + "\n")
print("\n".join(lines), flush=True)
