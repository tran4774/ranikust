---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0018: No proot in 1.0

## Context and Problem Statement

On the `--parallel-stages` path in kaniko mode, `RUN` executes in a bare
`chroot` with no `/proc` and, under rootless podman, no `/dev`
([ADR-0014](0014-kaniko-mode-sequential-by-default.md)). proot translates
paths through `ptrace` and can bind the real `/proc` and `/dev` into each
stage without mounting. Other mount-free techniques exist. Should ranikust
use one of them to give `RUN` a real `/proc` on this path?

## Considered Options

- Ship or invoke proot.
- Write a supervisor on seccomp user notification (`SECCOMP_RET_USER_NOTIF`)
  that translates paths like proot.
- fakechroot (`LD_PRELOAD`).
- None: bare `chroot` plus a static fake `/proc`.

## Decision Outcome

ranikust 1.0 neither ships nor invokes proot or any other `ptrace`/seccomp
path translator; the parallel kaniko-mode path uses a bare `chroot` with a fake
`/proc` ([kaniko-mode](../design/kaniko-mode.md)).

- Dockerfiles that need a real `/proc` (`/dev/fd`, `/proc/self/exe`, `ps`)
  run without `--parallel-stages`, which is the default.

### Consequences

- Good, because ranikust stays one binary under `MIT OR Apache-2.0`, with no
  third executable and no GPL component.
- Good, because the parallel path keeps its speed; proot would erase it on
  small machines.
- Bad, because the parallel path cannot run every Dockerfile; users must know
  when to leave `--parallel-stages` off.

## Evidence

- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md): proot
  added 26–56 % wall time on W1 and W3 (+3.5 to +6.2 s), ×1.3 (npm) to ×5
  (`find` over 250k files) per command; at 4 CPU on W3 (17.34 s) it erased
  the whole gain of parallel stages over sequential (17.14 s). `apt` hangs
  under proot unless `-o APT::Sandbox::User=root` is set. proot is a third
  binary under GPL-2.0 [external]. fakechroot does not work: static binaries
  ignore `LD_PRELOAD` and read the container's real files. A seccomp
  user-notification supervisor is possible (the building blocks are allowed
  by Docker's profile) but means reimplementing proot (`ADDFD`, every `*at()`
  call, symlinks, TOCTOU) [inferred].
- [Experiment 13](../research/experiments/13-docker-rootful-vm.md): Docker's
  `docker-default` AppArmor profile does not block proot's `ptrace`, and the
  `apt` hang reproduces on real Docker.

## More Information

Revisit after 1.0 if users need the parallel path for Dockerfiles that require
a real `/proc`; the seccomp user-notification route would avoid a third
binary.
