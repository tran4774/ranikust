# Runs as root in a `RUN --network=none` built with `--allow
# network.none-userns` where ranikust lacks CAP_NET_ADMIN (ADR-0033): the
# RUN's network namespace is owned by a user namespace of uid 0, so root
# holds every capability there; the seccomp filter must keep it from using
# that namespace beyond loopback. Used by tests/vm/guest/smoke.sh.
import ctypes
import errno
import fcntl
import os
import socket

libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER = 0x10000000
NS_GET_USERNS = 0xB701
NETLINK_NETFILTER = 12
NETLINK_XFRM = 6


def refused(ret, what):
    err = ctypes.get_errno()
    if ret != -1 or err != errno.EPERM:
        raise SystemExit(f"{what}: expected EPERM, got {ret} ({os.strerror(err)})")


assert os.getuid() == 0, "the checks need a root RUN"

# Loopback is up and works.
with open("/sys/class/net/lo/flags") as f:
    assert int(f.read(), 16) & 1, "lo is down"
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen()
socket.create_connection(server.getsockname()).close()

# (i) The user namespace that owns the network namespace: its fd can be
# had, but joining it is refused.
net = os.open("/proc/self/ns/net", os.O_RDONLY)
try:
    owner = fcntl.ioctl(net, NS_GET_USERNS)
except OSError as e:
    owner = None
    print(f"NS_GET_USERNS refused: {e}")
if owner is not None:
    refused(libc.setns(owner, CLONE_NEWUSER), "setns into the owning user namespace")
refused(libc.setns(net, 0), "setns into the network namespace")

# (ii) No new user namespace.
refused(libc.unshare(CLONE_NEWUSER), "unshare(CLONE_NEWUSER)")

# (iii) No nf_tables or xfrm; rtnetlink stays open.
for proto, name in [(NETLINK_NETFILTER, "NETLINK_NETFILTER"), (NETLINK_XFRM, "NETLINK_XFRM")]:
    try:
        socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, proto).close()
    except PermissionError:
        continue
    raise SystemExit(f"{name}: the socket opened")
socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0).close()

print("netns checks: ok")
