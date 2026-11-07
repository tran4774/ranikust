---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0134: With `--allow proc.rbind`, the PID namespace check reads files no ptrace check guards

## Context and Problem Statement

[ADR-0129](0129-with-allow-proc-rbind-a-shared-pid-namespace-is-refused-and-the-store-is-anonymous.md)
and
[ADR-0130](0130-with-allow-proc-rbind-the-residual-is-the-measured-one-the-peer-scan-stays-and-the-build-uses-the-persistent-store.md)
refuse, for a build whose `RUN`s get an rbind of `/proc`, a PID namespace shared
with a process of another mount namespace, through ADR-0101's peer scan, which
counts the processes whose `/proc/<pid>/root` ranikust can open (ptrace access);
ADR-0129 named the others an accepted residual. Under the rbind, what a `RUN`
reads of a process takes no ptrace access. How should the refusal of a shared
PID namespace be judged for such a build? A review of
[#79](https://github.com/tran4774/ranikust/issues/79) raised it.

## Considered Options

- Judge the PID namespace from files that no ptrace check guards, as a `RUN`
  reads them: PID 1's mount table and PID 2's `stat` (taken).
- Scan the mount table of every process, the same way: it would also refuse a
  second build or any process that entered a mount namespace of its own in the
  same container, which needs the maintainer's call; not taken.

## Decision Outcome

With `--allow proc.rbind`, ranikust also refuses, before it enters the build's
user namespace, a PID namespace that files with no ptrace check show shared:
PID 2 is a kernel thread, or PID 1, unless it is ranikust, lists no mount of the
mount namespace ranikust started in.

The maintainer chose (2026-11-08) to settle it in #79, with the first option,
the cheaper of the two the review proposed.

- **The host's PID namespace.** `/proc/2/stat` with `PF_KTHREAD` in its flags:
  `kthreadd`, PID 2 of the initial PID namespace only. No PID 2 passes.
- **PID 1 of another mount namespace.** ranikust reads its own
  `/proc/self/mountinfo` as it starts, before a set-up may enter a mount
  namespace of its own, and keeps the mount IDs. `/proc/1/mountinfo` must list
  one of them: a mount is in one namespace, and its ID is unique on the system.
  Where ranikust is PID 1 (`podman run … ranikust …`), this part passes.
- **What cannot be told is refused:** a file that cannot be read or parsed,
  each with its step, the error and guidance.
- **The peer scan stays** as ADR-0129 and ADR-0130 run it, after it, and as
  ADR-0101 runs it for kaniko mode on `/`, which this check does not change.
- **Scope.** The PID namespace of another container (`--pid container:<name>`,
  a pod with `shareProcessNamespace: true`), whose PID 1 is of another mount
  namespace, is refused by the second part. Where PID 1 is of ranikust's, a
  process that enters another mount namespace stays under ADR-0130's residual
  and the peer scan.
- **What a `RUN` reads**, as the flag's help, the guidance, the warning and the
  threat model now word it: the files of `/proc/<pid>` not under ptrace's check,
  such as `cmdline`, `status`, `stat`, `mountinfo`, `cgroup` and `net/*`. This
  replaces ADR-0130's closed list (command line, status, mount table and
  network tables) in those texts; ADR-0130 itself is unchanged.

### Consequences

- Good, because the refusal of a shared PID namespace reads what a `RUN`
  reads, whatever ranikust may open.
- Good, because both files are read once, before the build's user namespace.
- Bad, because ranikust started in a mount namespace other than its container's
  PID 1's (a CI agent that runs each step in a private mount namespace) is
  refused with `--allow proc.rbind`.
- Neutral, because the check runs only for a build whose probe chose the rbind.

## Evidence

- `/proc/<pid>/mountinfo`, `stat`, `status` and `cmdline` open with no ptrace
  check, while `root`, `environ` and `mem` take one **[code]** (Linux
  `fs/proc/base.c`); `stat` shows the task's flags to any reader **[code]**
  (`fs/proc/array.c`, `do_task_stat`).
- Mount IDs come from one allocator for the whole system **[code]**
  (`fs/namespace.c`, `mnt_alloc_id`).
- Unit tests in `ranikust` on fixture `mountinfo` and `stat` text, and the T1
  leg of the `rootless` smoke case: rootless Podman `--pid=host` with
  `--allow proc.rbind` is refused before any `RUN`.

## More Information

Supersedes, for the rbind only, ADR-0129's accepted residual of a process
ranikust cannot open; refines ADR-0129 and ADR-0130, which stay unchanged. Kaniko
mode on `/` keeps
[ADR-0101](0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)'s scan alone.
Implemented in `crates/ranikust/src/exposure.rs` (`check_pid_namespace`) and
`main.rs` (`refuse_shared_pid_namespace`); described in the threat model (T12,
the rootless row, section 6) and the book.
