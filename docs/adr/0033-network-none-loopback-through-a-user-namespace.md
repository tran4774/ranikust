---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0033: `RUN --network=none` without `CAP_NET_ADMIN` is refused, unless `--allow network.none-userns` opts in to a user namespace under seccomp

## Context and Problem Statement

A `RUN --network=none` runs in a new network namespace with only `lo`,
which must be up: tools reach themselves through `127.0.0.1`. Bringing it
up needs `CAP_NET_ADMIN` over the namespace. In a container with Docker's
or podman's default capabilities plus `SYS_ADMIN` (the `MS_MOVE`
environment of `ADR-0004`) ranikust has none, and the step fails with
`EPERM`. BuildKit always has loopback up through runc; it does not support
that container, so `ADR-0032` escalated the question
([Q-183](../open-questions.md#q-183-what-does-run---networknone-do-without-cap_net_admin)).

A network namespace owned by a new user namespace solves loopback, but a
host-safety review found what it exposes when the `RUN` itself stays in the
build's user namespace: that user namespace belongs to uid 0, and the
kernel gives a process of the parent user namespace whose euid owns a
child user namespace every capability in it (`cap_capable`). A root `RUN`
then holds, whatever its own capability sets:

- `CAP_NET_ADMIN` over its network namespace: nf_tables, xfrm, traffic
  control and the rest of the network configuration code, the attack
  surface of CVE-2022-1015, CVE-2023-32233 and CVE-2024-1086;
- the user namespace itself: `ioctl(NS_GET_USERNS)` on `/proc/self/ns/net`
  gives its descriptor, and `setns(CLONE_NEWUSER)` joins it (the check
  against a `chroot` exists only in `unshare`).

On hosts that block unprivileged user namespaces (Debian's
`unprivileged_userns_clone=0`, Ubuntu's AppArmor restriction) this hands
that surface to untrusted Dockerfiles. Another owner uid or nested user
namespaces do not help: the owner rule applies at the outermost level, and
root keeps `CAP_SETUID`.

## Considered Options

- (a) Refuse `RUN --network=none` there with a clear error.
- (b) Run the `RUN` itself inside the user namespace (rootless BuildKit's
  model): no capability over the parent, but a different uid mapping, file
  ownership through the map, and the rootless questions of Phase 3.
- (c) Keep the `RUN` in the build's user namespace, join a network
  namespace the new user namespace owns, and block with seccomp the calls
  that reach beyond loopback.
- (d) Run with loopback down, with a warning. Differs from BuildKit and
  fails `RUN`s that need `127.0.0.1` in an unclear way.

## Decision Outcome

By default ranikust refuses a `RUN --network=none` it cannot give loopback
itself (a); `--allow network.none-userns`, an entitlement in the style of
BuildKit's `--allow`, opts in to (c).

- Probe, once at the first `--network=none` `RUN`: a network namespace with
  `lo` up without a user namespace (root on a host, `--privileged`,
  rootless podman: the clone's own namespace, as before, no filter);
  otherwise, only with the flag, one owned by a new user namespace, and a
  child that joins it.
- Without the flag the refusal reads: `RUN --network=none: cannot bring up
  the loopback interface without CAP_NET_ADMIN (…); give ranikust
  CAP_NET_ADMIN, or pass --allow network.none-userns to use a user
  namespace under a seccomp filter (ADR-0033)`. With the flag but no user
  namespaces either, a second error names both causes.
- With the flag, a helper process creates the user namespace and the
  network namespace, brings `lo` up, sends a descriptor of the namespace
  (`/proc/self/ns/net`) to ranikust over a close-on-exec socket pair
  (`SCM_RIGHTS`) and exits; the descriptor keeps the namespace alive.
  The `RUN` child joins it with `setns` before any mount, keeps its uid,
  gid and Docker's capability set, and, after every mount and before the
  capability drop, installs a seccomp filter (`rnk_sandbox`'s `seccomp`
  module). The filter closes exactly this, each number checked against the
  kernel's uapi headers:
  - `EPERM` for `setns`, of any kind;
  - `EPERM` for `unshare` and `clone` with `CLONE_NEWUSER`;
  - `EPERM` for `io_uring_setup`, `io_uring_enter` and
    `io_uring_register`, whose operations would pass the rules below;
  - `EPERM` for `socket(AF_KEY)` (xfrm through PF_KEY);
  - `EPROTONOSUPPORT` for `socket(AF_NETLINK)` except `NETLINK_ROUTE`,
    `NETLINK_SOCK_DIAG`, `NETLINK_AUDIT` and `NETLINK_KOBJECT_UEVENT`:
    nf_tables, xfrm, generic netlink (IPVS, WireGuard and the rest) look
    absent, as on a kernel without them. Audit checks capabilities in the
    initial user namespace, and shadow's `useradd` and `passwd` abort
    without it, so it stays open, as in Docker's profile;
  - `EPERM` for `setsockopt` and `getsockopt` of: at `SOL_IP`,
    `IP_IPSEC_POLICY` and `IP_XFRM_POLICY` (16–17), ip_tables (64–67),
    arp_tables (96–99), ebtables (128–132), multicast routing (200–212)
    and IPVS (1152–1167); at `SOL_IPV6`, `IPV6_IPSEC_POLICY` and
    `IPV6_XFRM_POLICY` (34–35), ip6_tables (64–65 and 68–69, around
    `IPV6_RECVTCLASS` and `IPV6_TCLASS`, 66–67) and multicast routing
    (200–212); at `SOL_TCP`, `TCP_REPAIR` (19);
  - `EPERM` for every syscall of another architecture than the binary's
    (i386 and x32 on x86_64), whose numbers the rules would not match;
  - `ENOSYS` for `clone3`: a filter cannot read its flags, and `ENOSYS`
    makes the C libraries fall back to `clone`, as Docker's profile does.

  Every other interface that `CAP_NET_ADMIN` over that namespace opens
  stays open to a root `RUN`. The filter is installed without
  `no_new_privs`, so setuid binaries of the `RUN` keep working. A `RUN`
  that would join such a namespace on an architecture the filter does not
  know is refused.
- Planned: Phase 3 rootless work moves this path to (b), the `RUN` inside
  the user namespace, which removes the capabilities over the parent's
  child namespace instead of filtering their use.

### Consequences

- Good, because by default no `RUN` gains a capability it would not have
  under Docker.
- Good, because an operator who accepts the residual risk can build
  Dockerfiles with `RUN --network=none` in the `SYS_ADMIN`-only container.
- Bad, because with the flag rtnetlink stays open, and with it every
  other `CAP_NET_ADMIN`-gated interface the filter does not name: a root
  `RUN` can create and configure interfaces, addresses, routes and traffic
  control in its own network namespace, kernel code that has had
  vulnerabilities of its own (traffic control classifiers among them), and
  `ip link add type <kind>` makes the kernel load the `rtnl-link-<kind>`
  module into the host kernel if it is not loaded. `ip` and `getifaddrs`
  need rtnetlink, so closing it would break ordinary `RUN`s.
- Bad, because it is a denylist: an interface a later kernel gates on
  `CAP_NET_ADMIN` over the namespace is open until the filter names it.
- Neutral, because environments that already had `CAP_NET_ADMIN` behave
  as before, without the filter.

## Evidence

- BuildKit's `none` network provider leaves the new network namespace to
  runc, which brings `lo` up (`buildkit@cd8ee244 util/network/none.go`)
  [code], runc [external].
- `cap_capable` (`security/commoncap.c`) grants every capability in a user
  namespace to a process of its parent whose euid is its owner;
  `ns_get_owner` (`kernel/user_namespace.c`) gives `NS_GET_USERNS`
  (`fs/nsfs.c`) to a process of an ancestor user namespace [external, from
  the kernel source].
- The option ranges come from the uapi headers `linux/in.h`, `linux/in6.h`,
  `netfilter_ipv4/ip_tables.h`, `netfilter_ipv6/ip6_tables.h`,
  `netfilter_arp/arp_tables.h`, `netfilter_bridge/ebtables.h`,
  `linux/mroute.h`, `linux/mroute6.h`, `linux/ip_vs.h` and `linux/tcp.h`
  [code].
- `rnk-sandbox`'s T0 test `the_filter_refuses_what_it_should_and_nothing_else`
  installs the filter in a forked child and checks each refused and allowed
  call, every option range at both ends and just outside, with descriptor
  -1 so that `EPERM` can only come from the filter [measured]. The VM smoke
  suite's `sysadmin` variant, on kernels 5.15 and 6.8, with `ip_tables`,
  `ip6_tables` and `af_key` loaded: without the flag the refusal names it;
  with it, a root `RUN` sees loopback up and working; its `setns` into the
  owning user namespace, `unshare(CLONE_NEWUSER)`, `IPT_SO_SET_REPLACE`,
  `IPT_SO_GET_INFO`, `IP6T_SO_GET_INFO`, `TCP_REPAIR` and `socket(AF_KEY)`
  give `EPERM`, nf_tables and xfrm netlink `EPROTONOSUPPORT`, and
  `useradd` works (`tests/smoke/netns-check.py`) [measured,
  [testing](../testing.md#the-vm-harness-tier-t2)].

## More Information

- Answers Q-183. Related: `ADR-0004` (every privilege level), `ADR-0010`
  (`RUN` network host or none), `ADR-0032` (the BuildKit rule and
  `--allow network.host`). Threat model T20.
- How it works: [sandbox](../design/sandbox.md#per-run-sequence).
- Revisit with Phase 3's `--userns` (option (b)), or if a kernel
  vulnerability reachable through rtnetlink calls for closing it too.
