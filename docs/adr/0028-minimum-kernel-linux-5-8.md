---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0028: Require Linux 5.8 or later and check it at start-up

## Context and Problem Statement

`rnk-rootfs` confines every path taken from an image, a build context or a
Dockerfile to the build root by letting the kernel resolve it:
`openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` (Linux 5.6) and the
`statx` mount id (Linux 5.8), with no string-based fallback. On older kernels
such as RHEL/CentOS 8 (4.18), Debian 10 (4.19), Ubuntu 20.04's original
kernel (5.4) or Amazon Linux 2 (4.14), these calls do not exist, and today the
first filesystem operation fails, possibly in the middle of a build. kaniko
still runs on those kernels. What is the minimum kernel, and how does the user
learn about it? This was open question Q-054.

## Considered Options

- Require Linux 5.8, probe `openat2` and the `statx` mount id at start-up,
  and refuse with guidance.
- Require Linux 5.8 and document it only; fail at the first filesystem
  operation.
- Support older kernels with a user-space path resolver.

The user-space resolver would reverse `rnk-rootfs`'s central invariant: path
resolution in user space races against concurrent renames and symlink swaps
(TOCTOU), which is exactly what `RESOLVE_IN_ROOT` removes.

## Decision Outcome

ranikust requires Linux 5.8 or later; the binary probes `openat2` with
`RESOLVE_IN_ROOT` and the `statx` mount id at start-up, before any other work,
and stops with a message naming the running kernel and the requirement.

- The probe tests the calls themselves, not the version string, because a
  backported kernel may have them and seccomp may block them; a seccomp block
  gets its own message.
- The requirement is stated in the README and the user guide.
- `rnk-rootfs` keeps returning `Error::Unsupported` as a second line of
  defence.

### Consequences

- Good, because users on an unsupported kernel learn it in the first
  millisecond, not halfway through a build.
- Good, because the kernel, not ranikust, enforces path confinement.
- Bad, because RHEL/CentOS 8, Debian 10, Ubuntu 20.04 without the HWE kernel
  and Amazon Linux 2 with kernel 4.14 cannot run ranikust, while kaniko runs
  there.

## Evidence

- [external] `openat2(2)` and `RESOLVE_IN_ROOT` appeared in Linux 5.6;
  `STATX_MNT_ID` in Linux 5.8 (man pages `openat2(2)`, `statx(2)`).
- [measured] `crates/rnk-rootfs` escape suite: 61 T0 tests, including a
  thread swapping directories for symlinks during every operation, stay
  inside the root with `openat2`.
- [inferred] Docker 20.10 and later allow `openat2` and `statx` in their
  default seccomp profile; [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md)
  notes this without a source, and no experiment measured it. The start-up
  probe covers the case where it is blocked.

## More Information

- Answers Q-054 in [`docs/open-questions.md`](../open-questions.md#answered).
- Implemented by the start-up probe in `ranikust` (Phase 1 task 8);
  `rnk-rootfs` design: [rootfs](../design/rootfs.md).
- Revisit if a supported platform with a kernel older than 5.8 becomes a
  goal.
