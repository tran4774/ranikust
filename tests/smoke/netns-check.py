# Runs as root in a `RUN --network=none` built with `--allow
# network.none-userns` where ranikust lacks CAP_NET_ADMIN (ADR-0033): the
# RUN's network namespace is owned by a user namespace of uid 0, so root
# holds every capability there; the seccomp filter must keep it from the
# interfaces it closes. Used by tests/vm/guest/smoke.sh, which loads
# ip_tables first so that the x_tables checks reach the kernel's code.
import ctypes
import errno
import fcntl
import os
import socket

libc = ctypes.CDLL(None, use_errno=True)
CLONE_NEWUSER = 0x10000000
NS_GET_USERNS = 0xB701
SOL_IP = 0
SOL_IPV6 = 41
IPT_SO_SET_REPLACE = 64
IPT_SO_GET_INFO = 64
IP6T_SO_GET_INFO = 64
TCP_REPAIR = 19
AF_KEY = 15
PF_KEY_V2 = 2
NETLINK_XFRM = 6
NETLINK_AUDIT = 9
NETLINK_NETFILTER = 12


def refused(ret, what):
    err = ctypes.get_errno()
    if ret != -1 or err != errno.EPERM:
        raise SystemExit(f"{what}: expected EPERM, got {ret} ({os.strerror(err)})")


def raises(errnum, what, f):
    try:
        f()
    except OSError as e:
        if e.errno == errnum:
            return
        raise SystemExit(f"{what}: expected {errno.errorcode[errnum]}, got {e}")
    raise SystemExit(f"{what}: succeeded")


assert os.getuid() == 0, "the checks need a root RUN"

# Loopback is up and works.
with open("/sys/class/net/lo/flags") as f:
    assert int(f.read(), 16) & 1, "lo is down"
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen()
socket.create_connection(server.getsockname()).close()

# The user namespace that owns the network namespace: its descriptor can be
# had, but joining it is refused.
net = os.open("/proc/self/ns/net", os.O_RDONLY)
try:
    owner = fcntl.ioctl(net, NS_GET_USERNS)
    print(f"NS_GET_USERNS gave descriptor {owner}")
except OSError as e:
    owner = None
    print(f"NS_GET_USERNS refused: {e}")
if owner is not None:
    refused(libc.setns(owner, CLONE_NEWUSER), "setns into the owning user namespace")
refused(libc.setns(net, 0), "setns into the network namespace")
refused(libc.unshare(CLONE_NEWUSER), "unshare(CLONE_NEWUSER)")

# x_tables through socket options, xfrm through PF_KEY, TCP repair.
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
raises(errno.EPERM, "setsockopt(IPT_SO_SET_REPLACE)",
       lambda: udp.setsockopt(SOL_IP, IPT_SO_SET_REPLACE, b"\0" * 128))
raises(errno.EPERM, "getsockopt(IPT_SO_GET_INFO)",
       lambda: udp.getsockopt(SOL_IP, IPT_SO_GET_INFO, 84))
udp6 = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
raises(errno.EPERM, "getsockopt(IP6T_SO_GET_INFO)",
       lambda: udp6.getsockopt(SOL_IPV6, IP6T_SO_GET_INFO, 84))
tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
raises(errno.EPERM, "setsockopt(TCP_REPAIR)",
       lambda: tcp.setsockopt(socket.IPPROTO_TCP, TCP_REPAIR, 1))
raises(errno.EPERM, "socket(AF_KEY)", lambda: socket.socket(AF_KEY, socket.SOCK_RAW, PF_KEY_V2))

# nf_tables and xfrm netlink look absent; rtnetlink and audit stay open.
for proto, name in [(NETLINK_NETFILTER, "NETLINK_NETFILTER"), (NETLINK_XFRM, "NETLINK_XFRM")]:
    raises(errno.EPROTONOSUPPORT, name,
           lambda p=proto: socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, p))
socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0).close()
socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, NETLINK_AUDIT).close()

print("netns checks: ok")
