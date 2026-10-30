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
  child that joins it by pidfd.
- Without the flag the refusal reads: `RUN --network=none: cannot bring up
  the loopback interface without CAP_NET_ADMIN (…); give ranikust
  CAP_NET_ADMIN, or pass --allow network.none-userns to use a user
  namespace under a seccomp filter (ADR-0033)`. With the flag but no user
  namespaces either, a second error names both causes.
- With the flag, a holder process creates the user namespace and the
  network namespace, brings `lo` up and waits; the `RUN` child joins the
  network namespace through the holder's pidfd before any mount, keeps its
  uid, gid and Docker's capability set, and, after every mount and before
  the capability drop, installs a seccomp filter (`rnk_sandbox`'s
  `seccomp` module) that returns `EPERM` for:
  - `setns`, of any kind;
  - `unshare` and `clone` with `CLONE_NEWUSER`;
  - `socket(AF_NETLINK, …)` except `NETLINK_ROUTE`, `NETLINK_SOCK_DIAG`
    and `NETLINK_KOBJECT_UEVENT` (nf_tables, xfrm, generic netlink, audit
    and the rest are refused);
  - `io_uring_setup`, `io_uring_enter`, `io_uring_register`, whose
    `IORING_OP_SOCKET` would create sockets past the filter;
  - every syscall of another architecture than the binary's (i386 and x32
    on x86_64), whose numbers the rules would not match.

  `clone3` gets `ENOSYS` instead: a filter cannot read its flags, and
  `ENOSYS` makes the C libraries fall back to `clone`, as Docker's profile
  does. The filter is installed without `no_new_privs`, so setuid binaries
  of the `RUN` keep working.
- The holder is killed and reaped after the `RUN`.
- Planned: Phase 3 rootless work moves this path to (b), the `RUN` inside
  the user namespace, which removes the capabilities over the parent's
  child namespace instead of filtering their use.

### Consequences

- Good, because by default no `RUN` gains a capability it would not have
  under Docker.
- Good, because an operator who accepts the residual risk can build
  Dockerfiles with `RUN --network=none` in the `SYS_ADMIN`-only container.
- Bad, because with the flag rtnetlink stays open: a root `RUN` can create
  and configure interfaces, addresses, routes and traffic control in its
  own network namespace, kernel code reached with `CAP_NET_ADMIN` that
  has had vulnerabilities of its own (traffic control classifiers among
  them). `ip` and `getifaddrs` need rtnetlink, so closing it would break
  ordinary `RUN`s.
- Bad, because one extra process per such `RUN`, alive while it runs.
- Neutral, because environments that already had `CAP_NET_ADMIN` behave
  as before, without the filter.

## Evidence

- BuildKit's `none` network provider leaves the new network namespace to
  runc, which brings `lo` up (`buildkit@cd8ee244 util/network/none.go`)
  [code], runc [external].
- `cap_capable` (`security/commoncap.c`) grants every capability in a user
  namespace to a process of its parent whose euid is its owner; `ns_get_owner`
  (`kernel/nsproxy.c`, `fs/nsfs.c`) gives `NS_GET_USERNS` to a process of
  an ancestor user namespace [external, from the kernel source].
- `rnk-sandbox`'s T0 test `the_filter_refuses_what_it_should_and_nothing_else`
  installs the filter in a forked child and checks each refused and allowed
  call [measured]. The VM smoke suite's `sysadmin` variant, on kernels 5.15
  and 6.8: without the flag the refusal names it; with it, a root `RUN`
  sees loopback up and working, its `setns` into the owning user namespace,
  `unshare(CLONE_NEWUSER)` and `NETLINK_NETFILTER`/`NETLINK_XFRM` sockets
  all fail with `EPERM` (`tests/smoke/netns-check.py`) [measured,
  [testing](../testing.md#the-vm-harness-tier-t2)].

## More Information

- Answers Q-183. Related: `ADR-0004` (every privilege level), `ADR-0010`
  (`RUN` network host or none), `ADR-0032` (the BuildKit rule and
  `--allow network.host`). Threat model T9.
- How it works: [sandbox](../design/sandbox.md#per-run-sequence).
- Revisit with Phase 3's `--userns` (option (b)), or if a kernel
  vulnerability reachable through rtnetlink calls for closing it too.
