#!/usr/bin/env python3
"""Raw-frame probe of the T2 VM's egress NIC (ADR-0086), run as root in the
VM by guest/net.sh.

Root in the VM, a `--privileged` container and a ranikust `RUN` hold
CAP_NET_RAW, so they can hand passt any frame, not only what the guest's
routing would send. This probe does that: it writes Ethernet frames on the
egress NIC, addressed to the gateway's MAC (passt), with TCP SYNs and UDP
datagrams to the addresses that would land on the host's loopback if passt
forwarded them unchanged: 127.0.0.1, 0.0.0.0 and the guest's own address,
and in IPv6 frames ::1, ::ffff:127.0.0.1, :: and the guest's own address.
The port is the one where tests/vm/vm.sh serves its token on the host's
loopback, over TCP and UDP. A SYN-ACK, or any UDP datagram back, is a
reach.

Controls through the same socket, NIC and gateway, sent first: a SYN to
the image registry's port 443 must be answered with a SYN-ACK, and a DNS
query to the guest's DNS address with a datagram. passt runs IPv4 only, so
a SYN to the registry's IPv6 address, when it has one, must get no answer.
One frame at a time, and after each probe the kernel's own DNS query must
still be answered, so that a frame that stops passt shows as such rather
than as "no answer".

On the harness NIC (QEMU's user network with restrict=on, no egress),
--control names the one forward that answers a SYN: it is the only control
and the liveness check, and the same probes must go unanswered.

Prints one line per control ("control ok ..." or "CONTROL FAILED ...") and
per probe ("REACHED ..." or "ok ..."), then "PROBED". Exits 1 when a
control failed, 2 on a usage or set-up error.
"""

import argparse
import json
import os
import random
import select
import socket
import struct
import subprocess
import sys
import time

ETH_P_ALL = 0x0003
ETH_P_IP = 0x0800
ETH_P_IPV6 = 0x86DD
PACKET_OUTGOING = 4
TCP, UDP = 6, 17
SYN, ACK, RST = 0x02, 0x10, 0x04


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack("!%dH" % (len(data) // 2), data))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return ~total & 0xFFFF


def addr_bytes(family, addr):
    return socket.inet_pton(socket.AF_INET if family == 4 else socket.AF_INET6, addr)


def pseudo_header(family, src, dst, proto, length):
    if family == 4:
        return addr_bytes(4, src) + addr_bytes(4, dst) + struct.pack("!BBH", 0, proto, length)
    return addr_bytes(6, src) + addr_bytes(6, dst) + struct.pack("!I3xB", length, proto)


def ip_packet(family, src, dst, proto, payload):
    if family == 4:
        header = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), random.randrange(1 << 16),
                             0x4000, 64, proto, 0, addr_bytes(4, src), addr_bytes(4, dst))
        header = header[:10] + struct.pack("!H", checksum(header)) + header[12:]
        return header + payload
    return struct.pack("!IHBB16s16s", 6 << 28, len(payload), proto, 64,
                       addr_bytes(6, src), addr_bytes(6, dst)) + payload


def tcp_syn(family, src, dst, sport, dport):
    header = struct.pack("!HHIIBBHHH", sport, dport, random.randrange(1 << 32), 0, 5 << 4, SYN, 64240, 0, 0)
    csum = checksum(pseudo_header(family, src, dst, TCP, len(header)) + header)
    return header[:16] + struct.pack("!H", csum) + header[18:]


def udp_datagram(family, src, dst, sport, dport, data):
    length = 8 + len(data)
    csum = checksum(pseudo_header(family, src, dst, UDP, length) + struct.pack("!HHHH", sport, dport, length, 0) + data)
    return struct.pack("!HHHH", sport, dport, length, csum or 0xFFFF) + data


def dns_query(name):
    labels = b"".join(bytes([len(p)]) + p.encode() for p in name.rstrip(".").split(".")) + b"\0"
    return struct.pack("!HHHHHH", random.randrange(1 << 16), 0x0100, 1, 0, 0, 0) + labels + struct.pack("!HH", 1, 1)


def ip_json(*args):
    return json.loads(subprocess.run(["ip", "-j", *args], check=True, capture_output=True, text=True).stdout or "[]")


def own_addresses(iface):
    """The first global IPv4 address, global IPv6 address and link-local
    IPv6 address of iface, each None when it has none."""
    v4 = v6 = link6 = None
    for link in ip_json("addr", "show", "dev", iface):
        for a in link.get("addr_info", []):
            if a["family"] == "inet" and a.get("scope") == "global" and v4 is None:
                v4 = a["local"]
            elif a["family"] == "inet6" and a.get("scope") == "global" and v6 is None:
                v6 = a["local"]
            elif a["family"] == "inet6" and a.get("scope") == "link" and link6 is None:
                link6 = a["local"]
    return v4, v6, link6


def gateway_mac(iface, gateway):
    for _ in range(20):
        for n in ip_json("neigh", "show", "dev", iface):
            if n.get("dst") == gateway and n.get("lladdr"):
                return n["lladdr"]
        # Asks the kernel to resolve it: any datagram toward the gateway.
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            try:
                s.sendto(b"", (gateway, 9))
            except OSError:
                pass
        time.sleep(0.2)
    raise SystemExit(f"error: no MAC for the gateway {gateway} on {iface}")


def resolve(host, family):
    try:
        return socket.getaddrinfo(host, 443, family, socket.SOCK_STREAM)[0][4][0]
    except (socket.gaierror, IndexError):
        return None


def parse(frame):
    """(src, sport, dport, proto, tcp flags or None, payload) of an IPv4 or
    IPv6 frame carrying TCP or UDP, else None."""
    if len(frame) < 14:
        return None
    ethertype = struct.unpack("!H", frame[12:14])[0]
    pkt = frame[14:]
    if ethertype == ETH_P_IP and len(pkt) >= 20:
        ihl = (pkt[0] & 0x0F) * 4
        proto, src = pkt[9], socket.inet_ntop(socket.AF_INET, pkt[12:16])
        l4 = pkt[ihl:]
    elif ethertype == ETH_P_IPV6 and len(pkt) >= 40:
        proto, src = pkt[6], socket.inet_ntop(socket.AF_INET6, pkt[8:24])
        l4 = pkt[40:]
    else:
        return None
    if proto == TCP and len(l4) >= 20:
        sport, dport = struct.unpack("!HH", l4[:4])
        return src, sport, dport, TCP, l4[13], b""
    if proto == UDP and len(l4) >= 8:
        sport, dport = struct.unpack("!HH", l4[:4])
        return src, sport, dport, UDP, None, l4[8:]
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iface", required=True)
    ap.add_argument("--gateway", required=True, help="the IPv4 default gateway on --iface")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--dns", help="the guest's DNS server address")
    ap.add_argument("--registry", help="a host name answering on port 443")
    ap.add_argument("--control", metavar="ADDR:PORT",
                    help="for a NIC with no egress: a TCP port that answers a SYN, the only control"
                         " and the liveness check (instead of --dns and --registry)")
    ap.add_argument("--debug", action="store_true", help="print every TCP or UDP frame that comes in")
    args = ap.parse_args()
    if not args.control and not (args.dns and args.registry):
        ap.error("--dns and --registry, or --control")

    own_mac = bytes.fromhex(open(f"/sys/class/net/{args.iface}/address").read().strip().replace(":", ""))
    gw_mac = bytes.fromhex(gateway_mac(args.iface, args.gateway).replace(":", ""))
    own4, own6, link6 = own_addresses(args.iface)
    if own4 is None:
        raise SystemExit(f"error: no IPv4 address on {args.iface}")

    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETH_P_ALL))
    sock.bind((args.iface, 0))
    used = set()

    def new_port():
        while True:
            p = random.randrange(40000, 60000)
            if p not in used:
                used.add(p)
                return p

    # (label, kind, family, dst, dport, sport, payload)
    probes = []
    controls = []
    for dst in ["127.0.0.1", "0.0.0.0", own4]:
        probes.append((f"tcp {dst}:{args.port}", TCP, 4, dst, args.port, new_port(), b""))
        probes.append((f"udp {dst}:{args.port}", UDP, 4, dst, args.port, new_port(), args.token.encode()))
    silent = []
    if args.control:
        control_addr, control_port = args.control.rsplit(":", 1)
        controls.append((f"tcp {args.control} (control forward)", TCP, 4, control_addr, int(control_port), new_port(), b""))
    else:
        reg4 = resolve(args.registry, socket.AF_INET)
        if reg4 is None:
            raise SystemExit(f"error: {args.registry} has no IPv4 address")
        controls.append((f"tcp {reg4}:443 ({args.registry})", TCP, 4, reg4, 443, new_port(), b""))
        controls.append((f"udp {args.dns}:53 (DNS)", UDP, 4, args.dns, 53, new_port(), dns_query(args.registry)))
        # passt runs IPv4 only (ADR-0086): the registry over IPv6 must not
        # answer either.
        reg6 = resolve(args.registry, socket.AF_INET6)
        if reg6:
            silent.append((f"tcp [{reg6}]:443 ({args.registry})", TCP, 6, reg6, 443, new_port(), b""))
        else:
            print(f"note: {args.registry} has no IPv6 address: IPv6 egress checked by the probes only")
    # IPv6 frames go out whatever the guest's IPv6 set-up, from its own
    # address (global, else link-local, else a documentation one); none may
    # be answered.
    src6 = own6 or link6 or "2001:db8::2"
    for dst in ["::1", "::ffff:127.0.0.1", "::", src6]:
        probes.append((f"tcp [{dst}]:{args.port}", TCP, 6, dst, args.port, new_port(), b""))
        probes.append((f"udp [{dst}]:{args.port}", UDP, 6, dst, args.port, new_port(), args.token.encode()))

    # Liveness, checked before the first frame and after each probe, so that
    # a frame that stops the backend shows as such, not as "no answer": the
    # kernel's own DNS query gets an answer, or with --control, a new SYN to
    # the control port gets a SYN-ACK.
    if not args.control:
        kernel = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        kernel.bind((own4, 0))
        kernel.settimeout(3)

    def alive():
        if args.control:
            sport = new_port()
            send(TCP, 4, control_addr, int(control_port), sport, b"")
            got = listen(TCP, sport, 3)
            return got is not None and got[4] & (SYN | ACK) == SYN | ACK
        query = dns_query(args.registry)
        try:
            kernel.sendto(query, (args.dns, 53))
            while kernel.recv(4096)[:2] != query[:2]:
                pass
            return True
        except OSError:
            return False

    def send(kind, family, dst, dport, sport, payload):
        src = own4 if family == 4 else src6
        l4 = tcp_syn(family, src, dst, sport, dport) if kind == TCP else udp_datagram(family, src, dst, sport, dport, payload)
        ethertype = ETH_P_IP if family == 4 else ETH_P_IPV6
        sock.send(gw_mac + own_mac + struct.pack("!H", ethertype) + ip_packet(family, src, dst, kind, l4))

    def listen(kind, sport, seconds):
        """The first frame of that protocol back to sport within seconds."""
        deadline = time.monotonic() + seconds
        while (left := deadline - time.monotonic()) > 0:
            if not select.select([sock], [], [], left)[0]:
                continue
            frame, addr = sock.recvfrom(65535)
            if addr[2] == PACKET_OUTGOING:
                continue
            got = parse(frame)
            if args.debug and got:
                print(f"debug: {got[0]} {got[1]} -> {got[2]} proto {got[3]} flags {got[4]}")
            if got and got[2] == sport and got[3] == kind:
                return got
        return None

    print(f"raw frames from {own4} on {args.iface} to the gateway's MAC {gw_mac.hex(':')}")
    if not alive():
        print("CONTROL FAILED liveness: no answer before any raw frame")
        print("PROBED")
        return 1
    failed = False
    for label, kind, family, dst, dport, sport, payload in controls:
        send(kind, family, dst, dport, sport, payload)
        got = listen(kind, sport, 3)
        ok = got is not None and (kind == UDP or got[4] & (SYN | ACK) == SYN | ACK)
        print(f"{'control ok' if ok else 'CONTROL FAILED'} raw {label}")
        failed |= not ok
    for label, kind, family, dst, dport, sport, payload in silent:
        send(kind, family, dst, dport, sport, payload)
        got = listen(kind, sport, 3)
        ok = got is None
        print(f"{'control ok' if ok else 'CONTROL FAILED'} raw {label}: IPv6 egress {'off' if ok else 'ON'}")
        failed |= not ok
    for label, kind, family, dst, dport, sport, payload in probes:
        send(kind, family, dst, dport, sport, payload)
        got = listen(kind, sport, 1.5)
        if got is None:
            print(f"ok   raw {label}: no answer")
        elif kind == UDP:
            print(f"REACHED raw {label}: a datagram from {got[0]}: {got[5][:64]!r}")
        elif got[4] & (SYN | ACK) == SYN | ACK:
            print(f"REACHED raw {label}: SYN-ACK from {got[0]}")
        else:
            print(f"ok   raw {label}: refused (flags {got[4]:#x} from {got[0]})")
        if not alive():
            print(f"CONTROL FAILED liveness: the backend stopped answering after raw {label}")
            failed = True
            break
    print("PROBED")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
