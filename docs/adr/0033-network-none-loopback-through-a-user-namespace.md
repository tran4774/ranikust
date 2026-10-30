---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0033: `RUN --network=none` gets loopback through a user namespace where `CAP_NET_ADMIN` is missing

## Context and Problem Statement

A `RUN --network=none` runs in a new network namespace with only `lo`,
which must be up: tools reach themselves through `127.0.0.1`. Bringing it
up needs `CAP_NET_ADMIN` over the namespace. In a container with Docker's
or podman's default capabilities plus `SYS_ADMIN` (the `MS_MOVE`
environment of `ADR-0004`), the process has none, and the step failed with
`EPERM`. BuildKit always has loopback up: runc brings it up, inside a user
namespace when BuildKit runs rootless. BuildKit does not support the
`SYS_ADMIN`-only container, so `ADR-0032` escalated the question
([Q-183](../open-questions.md#q-183-what-does-run---networknone-do-without-cap_net_admin)).

## Considered Options

- (a) Refuse `RUN --network=none` there with a clear error.
- (b) Run with loopback down, with a warning. Differs from BuildKit, and a
  `RUN` that needs `127.0.0.1` fails in an unclear way.
- (c) Create the network namespace inside a user namespace, whose root has
  `CAP_NET_ADMIN` over it, as rootless BuildKit does through runc.

## Decision Outcome

Where the build process cannot bring up `lo` itself, ranikust creates the
`RUN`'s network namespace inside a user namespace of its own and brings
`lo` up there (c); where user namespaces are denied too, it refuses with
an error naming both causes (a).

- The probe runs once, at the first `--network=none` `RUN`: a network
  namespace with `lo` up without a user namespace, else with one, else
  neither. A process that can do it alone (root on a host, `--privileged`,
  rootless podman) keeps the clone's own network namespace, as before.
- Only the network namespace is owned by the new user namespace. A holder
  process creates both, brings `lo` up and waits; the `RUN` child joins
  that network namespace through the holder's pidfd (`setns`, Linux 5.8)
  before any mount, and stays in the build's user namespace: its uid, gid,
  groups, mount, PID, UTS and IPC namespaces and Docker's capability set
  are unchanged. The holder is killed and reaped after the `RUN`.
- The refusal reads: `RUN --network=none: cannot bring up the loopback
  interface: without CAP_NET_ADMIN (…) and without a user namespace (…);
  give ranikust CAP_NET_ADMIN or allow user namespaces`.

### Consequences

- Good, because Dockerfiles with `RUN --network=none` build in the
  `SYS_ADMIN`-only container, as with rootless BuildKit.
- Good, because environments that already had `CAP_NET_ADMIN` behave
  exactly as before.
- Bad, because a root `RUN` gains every capability over its own network
  namespace there: the user namespace belongs to uid 0, and a process of
  the parent user namespace with that euid holds all capabilities in it.
  The namespace holds only `lo` and reaches nothing, so this changes no
  host exposure (threat model T9).
- Bad, because one extra process per such `RUN`, alive while it runs.
- Neutral, because seccomp profiles that deny `unshare(CLONE_NEWUSER)` or
  a sysctl that disables user namespaces lead to the refusal (a).

## Evidence

- BuildKit's `none` network provider leaves the new network namespace to
  runc, which brings `lo` up; rootless BuildKit runs runc inside a user
  namespace (`buildkit@cd8ee244 util/network/none.go`) [code], runc
  [external].
- The kernel grants a process every capability in a child user namespace
  whose owner is its euid (`cap_capable`, `kernel/user_namespace.c`), and
  `setns` into a network namespace needs `CAP_SYS_ADMIN` over its owning
  user namespace and the caller's [external].
- The VM smoke suite builds `tests/smoke/e2e` with `RUN --network=none`
  checks (only `lo`, and `lo` up) in `docker run --cap-add SYS_ADMIN`
  without `NET_ADMIN`, on kernels 5.15 and 6.8; the T2 tests
  `a_run_joins_a_held_network_namespace` and
  `root_brings_up_loopback_itself_and_in_a_user_namespace` cover the two
  paths as root [measured, [testing](../testing.md#the-vm-harness-tier-t2)].

## More Information

- Answers Q-183. Related: `ADR-0004` (every privilege level), `ADR-0010`
  (`RUN` network host or none), `ADR-0032` (the BuildKit rule).
- How it works: [sandbox](../design/sandbox.md#per-run-sequence).
- Revisit if the user-namespace path fails in a common environment, or if
  `--userns` (Phase 3) moves every `RUN` into a user namespace.
