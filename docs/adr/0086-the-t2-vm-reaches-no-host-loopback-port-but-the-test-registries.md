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
- **(d) A host firewall or a network namespace around QEMU**: needs root or
  a change to the host's network, which the harness may not make
  ([rules](../agents/rules.md): no `sudo`, `unshare` or host settings).

## Decision Outcome

Chosen: (b). The guest has two NICs, both started by `vm.sh` as the
invoking user, and the host's loopback ports it may reach are named in one
place at the top of `vm.sh` (`REGISTRY_PORTS`, `GUEST_DNS`).

- **Egress**: QEMU's own passt backend (`-netdev passt`, QEMU 10.1 and
  later), which starts passt on a socket pair (`--fd`), with
  `map-host-loopback=none`, `map-guest-addr=none` and no inbound port. No
  guest address names the host's loopback; the guest's own address (passt
  copies the host's) stays the guest's. passt quits when its QEMU goes; the
  teardown kills one still running (told by its DNS address, which only
  the harness gives passt; one VM at a time) and fails the run if it
  cannot.
- **DNS**: the guest's resolver is `192.0.2.53` (a documentation address,
  RFC 5737), which passt forwards (ports 53 and 853 only) to the host's
  first IPv4 nameserver in `/etc/resolv.conf`, usually systemd-resolved's
  `127.0.0.53`. The only other loopback port the guest reaches.
- **Harness**: QEMU's user network with `restrict=on` and no IPv6,
  `net=10.0.2.0/24`, its own address moved to `10.0.2.254` (a `guestfwd`
  may not take it). It forwards SSH from the host's `127.0.0.1:<free port>`
  to `10.0.2.15:22`, and, only with the push suite, `10.0.2.2:5000` and
  `10.0.2.2:5443` to the host's `127.0.0.1` of the same port, each
  connection through its own `socat`. Nothing else leaves through it, not
  even DNS. The guest keeps `10.0.2.2` for the registries, so the push
  suite and the registries' certificate (whose SANs hold it) are unchanged.
- **Guest**: a `network-config` in the run's seed tells the NICs apart by
  fixed MACs: DHCP on egress, a fixed `10.0.2.15/24` with no gateway, no
  DNS and no IPv6 on the harness NIC. The provisioning boot of the base
  image has the egress NIC only.
- **Proof**: `tests/vm/guest/net.sh` runs before any suite. `vm.sh` serves
  a random token on a free port of `127.0.0.1` (and `::1`); root in the VM,
  a default and a `--privileged` `docker run`, and a ranikust `RUN` probe
  every address that could name the host (`10.0.2.2`, `10.0.2.3`,
  `10.0.2.254`, `192.0.2.53`, the default gateways, the guest's own
  addresses, over IPv6 too from root). Any answer fails the run, and so
  does a failed image pull or HTTPS request; with the push suite, the
  registry port must answer from each.
- **Prerequisites**: QEMU with the passt backend (10.1 or later) and passt
  with `--map-host-loopback` (2024_08_21 or later) always, `socat` with the
  push suite; `vm.sh` checks them. It also refuses a host whose default
  route's network overlaps `10.0.2.0/24`, which passt would hand the
  guest.

Not covered: the host's non-loopback addresses (its LAN address other than
the one passt hands the guest, `docker0`, a VPN's) stay reachable from the
VM as they were, through passt as through libslirp before, like from any
machine on those networks.

### Consequences

- Good, because a hostile image or `RUN` in the VM no longer reaches the
  workstation's loopback-only services; only the two dummy registries, and
  only while the push suite runs.
- Good, because the check runs on every `just test-vm`, so a passt or QEMU
  that changes its defaults fails the run instead of opening the hole again.
- Good, because neither the registries, their certificate nor the push
  suite change.
- Bad, because the harness now needs QEMU 10.1 or later and passt on the
  host (and socat for `push`): an older QEMU (Ubuntu 24.04's 8.2, Debian
  13's 10.0) is refused.
- Neutral, because egress from the VM is as before: passt copies the
  host's address and routes, as libslirp NATed.

## Evidence

- Before, on `-netdev user` (the old `vm.sh` with `tests/vm/guest/net.sh`,
  `RANIKUST_QUICK_CASES=minimal just test-vm quick`, Ubuntu 24.04 guest,
  kernel 6.8, QEMU 10.2.1): root in the VM, a default and a `--privileged`
  `docker run` and a ranikust `RUN` each read the token from the host's
  `127.0.0.1` at `10.0.2.2` [measured].
- After, same run: none of them reaches it at any of `10.0.2.2`,
  `10.0.2.3`, `10.0.2.254`, `192.0.2.53`, the gateway, the guest's own
  addresses, nor over IPv6 (the gateway `fe80::1` and the guest's global
  addresses, the token also on `::1`); the image pull and HTTPS egress
  work from the VM and from the `RUN`; SSH is up 10 s after boot, as
  before [measured].
- With the change, on noble: `unit smoke quick secrets` (46/46 T2 tests),
  `e2e` (W0, every builder) and `push` (the registries at `10.0.2.2:5000`
  and `:5443` answer from root, a default container and a `RUN`, while the
  probe port at `10.0.2.2` stays out of reach) pass; so do a base image
  provisioned over passt alone (64 s) and booted, the plain cloud image
  (`RANIKUST_VM_GOLDEN=0`, apt at boot), and a jammy guest (kernel 5.15).
  No passt or socat process outlives a run [measured].
- passt 0.0~git20260120 on its own listening socket accepted no client
  (`Failed to add fd to epoll: Bad file descriptor`, also without
  `--one-off`); on a socket pair (`--fd`, which QEMU's backend uses) it
  works [measured on the host, no VM].
- passt's man page: `--map-host-loopback none` maps no address and implies
  `--no-map-gw`; with it, a loopback resolver is not handed to the guest
  ("Couldn't get any nameserver address"), hence `--dns-forward` and
  `--dns-host` [measured on the host, no VM].
- libslirp: with `restrict=on`, a guest TCP connection is dropped unless a
  `guestfwd` names its address and port; a `guestfwd` may not use the
  user network's own address or its DNS address [external: libslirp
  `tcp_input.c`, `slirp_add_exec`].

## More Information

- Fixes lead 28 of the [Phase 1-2 audit](../security/audit/README.md).
- Implemented in `tests/vm/vm.sh` (network section, `start_passt`,
  `start_qemu`, the run's `network-config`) and `tests/vm/guest/net.sh`;
  described in [docs/testing.md](../testing.md#the-vm-harness-tier-t2).
- Revisit if the harness must run on a QEMU older than 10.1: passt can then
  be started by `vm.sh` on a socket pair and handed to `-netdev stream`
  (`addr.type=fd`), not on its own listening socket, which failed here.
