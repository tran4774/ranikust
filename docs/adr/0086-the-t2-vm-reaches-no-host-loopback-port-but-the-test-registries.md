---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0086: The T2 VM reaches no port of the host's loopback but the test registries, through passt and a restricted user network

## Context and Problem Statement

The T2 VM (`tests/vm/vm.sh`) runs third-party code as root: images behind
mutable tags pulled through `mirror.gcr.io`, their `RUN` steps, `docker run
--privileged`, and in `e2e` and `ultra` npm installs. Its only NIC was
QEMU's user network (`-netdev user`, libslirp), which turns a guest
connection to `10.0.2.2:<port>` into a connection from the developer's QEMU
process to the host's `127.0.0.1:<port>`, for every port. The harness relied
on that for the push suite's test registries, but nothing scoped it, so any
code in the VM reached every loopback-only service of the workstation: lead
28 of the [Phase 1-2 security audit](../security/audit/phase-1-2/REPORT.md).
The question is how the VM keeps its egress (image pulls, apt, e2e fetches),
SSH from the host, and the two registry ports, and loses the rest of the
host's loopback.

## Considered Options

- **(a) User network with `restrict=on` and `guestfwd` rules only**: no
  egress at all, so every image pull, apt and npm fetch would need a proxy
  forwarded in by hand.
- **(b) passt for egress, a restricted user network for the harness**
  (chosen): passt maps nothing to the host's loopback when told so, and the
  restricted user network forwards only what the harness names.
- **(c) A firewall inside the guest** (nftables dropping `10.0.2.2` except
  5000 and 5443): root in the guest, which the suites hand to third-party
  code, removes it.
- **(d) A host firewall or a network namespace around QEMU**: a firewall
  needs root or a change to the host's network, which the harness may not
  make ([rules](../agents/rules.md): no `sudo`, `unshare` or host
  settings). A namespace made by pasta needs no root but a user namespace
  on the host: kept as a follow-up (below), not the fix.

## Decision Outcome

Chosen: (b), with passt in a reduced configuration. The guest has two
NICs, both started by `vm.sh` as the invoking user, and the host's
loopback ports it may reach are named in one place at the top of `vm.sh`
(`REGISTRY_PORTS`, `GUEST_DNS`, `HOST_DNS`).

- **Egress**: QEMU's own passt backend (`-netdev passt`, QEMU 10.1 and
  later), which starts passt on a socket pair (`--fd`), with
  `map-host-loopback=none`, `map-guest-addr=none`, no inbound port, and
  **IPv4 only** (`ipv6=off`, passt's `--ipv4-only`). No guest address names
  the host's loopback; the guest's own address (passt copies the host's)
  stays the guest's. passt quits when its QEMU goes; the teardown stops one
  still running (its pid from the pid file QEMU has it write, TERM then
  KILL) and fails the run if it cannot.
- **Why a reduced configuration** (decided 2026-11-03, after the first
  review): in passt's default configuration, a class of raw frame reached
  the host's loopback, which the raw-frame probe below found. The egress
  runs with a reduced configuration, which the probe confirms on every
  run; details are withheld until the passt maintainers have them. The VM
  loses nothing it needs.
- **DNS**: the guest's resolver is `192.0.2.53` (a documentation address,
  RFC 5737), which passt forwards to `HOST_DNS`, the host's first IPv4
  nameserver in `/etc/resolv.conf`, usually systemd-resolved's
  `127.0.0.53`: ports 53 and 853, TCP and UDP, on that address only. The
  only other loopback service the guest reaches. That resolver answers the
  host's own names too (`/etc/hosts`, split DNS, LLMNR, mDNS), as it did
  through QEMU's user network before.
- **Harness**: QEMU's user network with `restrict=on` and no IPv6,
  `net=10.0.2.0/24`, its own address moved to `10.0.2.254` (a `guestfwd`
  may not take it). It forwards SSH from the host's `127.0.0.1:<free port>`
  to `10.0.2.15:22`. With the push suite, a `guestfwd` for `10.0.2.2:5000`
  and `10.0.2.2:5443` starts a forwarder per guest connection, which hands
  it to the host's `127.0.0.1` of the same port only while a gate is open.
  `vm.sh` runs the push suite last, in an SSH step of its own after every
  other suite, and opens the gate only around that step: no text the
  guest writes moves it. The gate's state lives in `vm.sh` and moves one
  way only (shut, open, done; done never reopens), and when it shuts,
  `vm.sh` kills every forwarder still running, so no connection made while
  it was open outlives it. Each forwarder takes one of 16 `flock` slots per
  port and runs `socat` under `timeout 600`. Nothing else leaves through
  this NIC, not even DNS. The guest keeps `10.0.2.2` for the registries,
  so the registries' certificate (whose SANs hold it) is unchanged.
- **Guest**: a `network-config` in the run's seed tells the NICs apart by
  fixed MACs: DHCP on egress, a fixed `10.0.2.15/24` with no gateway, no
  DNS and no IPv6 on the harness NIC. The provisioning boot of the base
  image has the egress NIC only.
- **Proof**: `tests/vm/guest/net.sh` runs before any suite. `vm.sh` serves
  a random token over TCP and UDP on a free port of `127.0.0.1` (and
  `::1`). Root in the VM, a default and a `--privileged` `docker run`, and
  a ranikust `RUN` probe every address that could name the host
  (`10.0.2.2`, `10.0.2.3`, `10.0.2.254`, `192.0.2.53`, the default gateway,
  the guest's own addresses), and the two registry ports, which must not
  answer then. Each vantage must also reach the internet with the tool of
  its probe (curl for root in the VM, whose own busybox `wget` segfaults
  on jammy; alpine's busybox `wget` elsewhere) and print that its probe
  ran through; one that does not, or does not run, fails the run. Root also writes raw frames straight
  to each NIC's backend (`tests/vm/guest/rawprobe.py`; `CAP_NET_RAW`,
  which a `--privileged` container or a `RUN` on the VM's network also
  holds): TCP and UDP to the loopback, unspecified and own addresses, in
  IPv4 and IPv6 frames, one at a time with a liveness check after each.
  On the egress NIC, controls through the same socket come first (a SYN to
  the registry, a DNS query), and the registry must get no answer over
  IPv6; on the harness NIC the control is a `guestfwd` on `10.0.2.2:9` to
  QEMU's null character device, which reaches nothing on the host and
  starts no process but answers a SYN.
  The egress NIC must have no global IPv6 address and no IPv6 default
  route. The push suite then checks both registry ports from root, both
  containers and a `RUN`.
- **Prerequisites**: QEMU 10.1 or later with the passt backend and its
  options, passt 2024_10_30 (Debian's 0.0~git20241030) or later, the first
  with `--dns-host`, and `socat`, `flock` and `timeout` for the push suite;
  `vm.sh` checks each option of both and the version before boot, and that
  passt runs with `--ipv4-only` after. It also refuses a host whose default
  route's network overlaps `10.0.2.0/24`, which passt would hand the guest.

Not covered: the host's non-loopback addresses (its LAN address, the one
passt hands the guest included, which raw frames to the guest's own address
reach; `docker0`; a VPN's) stay reachable from the VM, as they were through
libslirp, like from any machine on those networks, except that the
connections come from the host itself.

### Consequences

- Good, because a hostile image or `RUN` in the VM no longer reaches the
  workstation's loopback-only services: only the host's resolver, and the
  two dummy registries while the push suite runs.
- Good, because the check runs on every `just test-vm`, raw frames
  included, so a passt or QEMU that changes its behaviour fails the run
  instead of opening the hole again.
- Good, because neither the registries nor their certificate change.
- Bad, because the harness now needs QEMU 10.1 or later and a recent passt
  on the host (and socat for `push`): an older QEMU (Ubuntu 24.04's 8.2,
  Debian 13's 10.0) is refused.
- Bad, because the push suite always runs last, whatever order the suites
  were named in.
- Bad, because every guest connection to a registry port starts a host
  `sh` (libslirp's `guestfwd` command), at no bounded rate, during the
  whole run: an accepted residual. While the gate is shut it exits at
  once; while it is open, the slots and the timeout bound what it runs.
- Neutral, because egress from the VM is as before but IPv4 only: passt
  copies the host's address and routes, as libslirp NATed.

## Evidence

- Before, on `-netdev user` (the old `vm.sh` with `tests/vm/guest/net.sh`,
  `RANIKUST_QUICK_CASES=minimal just test-vm quick`, Ubuntu 24.04 guest,
  kernel 6.8, QEMU 10.2.1, libslirp 4.9.1): root in the VM, a default and a
  `--privileged` `docker run` and a ranikust `RUN` each read the token from
  the host's `127.0.0.1` at `10.0.2.2`; raw frames to the loopback and
  unspecified addresses, over IPv4 and IPv6, TCP and UDP, reached it too
  [measured].
- With passt's default configuration (passt 0.0~git20260120): no ordinary
  socket in the VM reached the token, but one class of raw frame did, with
  an answer from the host's loopback; passt stayed up throughout
  [measured; details withheld until reported upstream].
- With the reduced configuration: every raw frame of the probe goes
  unanswered on the egress NIC, except that a TCP SYN to the guest's own
  address is refused by the host's own LAN address (see "Not covered"); the
  controls pass. On the restricted harness NIC, libslirp refuses the TCP
  SYNs itself (a RST) and drops the rest; its control forward answers
  [measured].
- passt 0.0~git20260120 on its own listening socket accepted no client
  (`Failed to add fd to epoll: Bad file descriptor`, also without
  `--one-off`); on a socket pair (`--fd`, which QEMU's backend uses) it
  works [measured on the host, no VM].
- passt's man page: `--map-host-loopback none` maps no address and implies
  `--no-map-gw`; with it, a loopback resolver is not handed to the guest
  ("Couldn't get any nameserver address"), hence `--dns-forward` and
  `--dns-host` [measured on the host, no VM]. `--map-host-loopback` and
  `--map-guest-addr` appeared in 0.0~git20240821, `--dns-host` in
  0.0~git20241030 [external: Debian's passt changelog].
- libslirp 4.9.1: with `restrict=on`, a guest TCP connection is dropped
  unless a `guestfwd` names its address and port; a `guestfwd` may not use
  the user network's own address or its DNS address [external: libslirp
  `tcp_input.c`, `slirp_add_exec`].
- With the change, on noble: `unit smoke quick secrets` (46/46 T2 tests),
  `e2e` (W0, every builder) and `push` pass, the network check and both
  raw passes first in each. In `push quick`, `quick` runs first with the
  registry ports shut, then `push` in its own step: the gate opens, both
  ports answer from root, both containers and the build's `RUN`, and the
  gate shuts after the step. `quick` also passes on a jammy guest (kernel
  5.15), on the plain cloud image (`RANIKUST_VM_GOLDEN=0`, packages
  installed over passt at boot), and on a base image provisioned over
  passt alone (75 s) in a private cache. No passt or socat process
  outlives a run [measured].

## More Information

- Fixes lead 28 of the [Phase 1-2 audit](../security/audit/README.md).
- Implemented in `tests/vm/vm.sh` (network section, `EGRESS_NETDEV`,
  `start_qemu`, the registry forwarder, `gate_open` and `gate_close`, the
  push step, the run's `network-config`), `tests/vm/guest/run.sh`
  (`--after`), `tests/vm/guest/net.sh` and `tests/vm/guest/rawprobe.py`;
  described in [docs/testing.md](../testing.md#the-vm-harness-tier-t2).
- Follow-ups: run passt in its own network namespace under pasta (defense
  in depth; needs a user namespace on the host, checked against the rules
  first); passt's default configuration again once it is fixed upstream and
  the raw probe passes with it.
- Revisit if the harness must run on a QEMU older than 10.1: passt can then
  be started by `vm.sh` on a socket pair and handed to `-netdev stream`
  (`addr.type=fd`), not on its own listening socket, which failed here.
