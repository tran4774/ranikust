---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0074: Through `/proc/<pid>/root` of a full-sandbox `RUN`, setuid files are `nosuid`; corrects ADR-0047

## Context and Problem Statement

[ADR-0047](0047-kaniko-workdir-chain-is-private.md) (accepted 2026-10-31)
records, among its Consequences, that while a `RUN` runs as a non-root `USER`,
a host user with the same uid can open `/proc/<pid>/root` of that process and
reach the workdir without passing the private ancestor, and that "a
setuid-root file from the image then works, since the host filesystem honours
setuid. Docker without userns-remap behaves the same, and overlay mode has it
too; only user namespaces or `nosuid` close it."

A source review after Phase 2 found the setuid half wrong for the full
sandbox. The `RUN` there lives in a mount namespace of its own, so the mounts
the same-uid user reaches through `/proc/<pid>/root` are foreign to it, and
the kernel treats a foreign mount as `nosuid`. ADR-0047's body cannot be
edited. Which record carries the correction?

## Considered Options

- Edit the Consequences of ADR-0047: not allowed; the body of an accepted ADR
  is never edited ([README](README.md#status-lifecycle)).
- Leave the statement: a reader would take a setuid file in a build root for
  a path to uid 0 for any account sharing a `USER`'s uid, in every mode.
- A new ADR that corrects the statement, with the corrected text in the
  threat model.

## Decision Outcome

The setuid consequence of ADR-0047 is corrected as follows: through
`/proc/<pid>/root` of a full-sandbox `RUN`, a setuid-root file from the image
does not take effect for a host user, because the kernel treats a mount of
another mount namespace as `nosuid`, and a hardlink out of that root fails
with `EXDEV`; the rest of ADR-0047 stands. Measured in the T2 VM, in overlay
mode, on 2026-11-03, on kernels 5.15 and 6.8 (see Evidence).

- **What stays true.** While the `RUN` lives as a non-root `USER`, a distinct
  host account can still read its `/proc/<pid>/environ` and reach its root
  through `/proc/<pid>/root`, with that account's file permissions, until
  `--userns` (Phase 3, [ADR-0003](0003-dockerfile-trust-and-default-isolation.md)),
  but only when both conditions hold [measured]:
  - the account's uid **and** primary gid both equal the `RUN`'s. A
    `USER <uid>` with no passwd entry runs as gid 0, so an account
    `<uid>:<uid>` gets `EACCES` on everything and can only list the
    descriptor numbers in `/proc/<pid>/fd` (a uid match is enough for that
    listing, which shows names only); `USER <uid>:<uid>` makes the account
    `<uid>:<uid>` the one that gets in. A different uid is always refused;
  - the `RUN` process is dumpable. A binary that is exec-only (`0711`)
    makes it non-dumpable, and then nothing is reachable.

  Files only root may read (`0600`) stay unreadable through the root. The
  threat model lists the reach as an accepted risk
  ([section 6](../security/threat-model.md#6-accepted-risks)).
- **Where ADR-0047's sentence was true.** Only for a `RUN` in the host's own
  mount namespace, by the same kernel rule: [measured] for a plain host
  process in the account's own mount namespace, where a setuid-root file
  executed through `/proc/<pid>/root` gives `euid=0`; the `Direct` `chroot` of
  a host `--workdir` that ADR-0047 first described has not been run, so for
  it the rule is [inferred]. [ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md)
  removed that workdir, and the binary runs a `Direct` `RUN` on `/` of a
  container only, so no mode has that case today.
- **The private chain stands.** Its purpose, that no other user reaches the
  image's setuid files by path or hardlinks one out, is unchanged; ADR-0050
  moved the chain to the state directory.

### Consequences

- Good, because the threat model no longer tells a reader that a same-uid
  account can turn an image's setuid file into privilege through
  `/proc/<pid>/root`, and the kernel rule behind that is measured, not
  inferred, for the full sandbox in overlay mode.
- Bad, because the measurement did not cover everything the sentence reaches.
  Still [inferred]: kaniko mode with a directory per stage; the `Direct`
  `chroot` case of ADR-0047; "Docker without user-namespace remapping behaves
  the same"; kernels other than 5.15 and 6.8; and `--userns`. The
  maintainer accepted it on 2026-11-03 with these gaps open.
- Neutral, because nothing in the code changes: it never depended on the
  statement.

## Evidence

- `RUN` in a mount namespace of its own, no user namespace [code]:
  `clone_flags` in `crates/rnk-sandbox/src/spawner.rs` gives a full-sandbox
  `RUN` `CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS | CLONE_NEWIPC` (and
  `CLONE_NEWNET` for `--network=none`), never `CLONE_NEWUSER`;
  `switch_user_and_exec` in `crates/rnk-sandbox/src/child.rs` switches to the
  `USER`'s raw ids with `set_thread_res_uid`.
- `Direct` only on `/` [code]: `crates/ranikust/src/setup.rs` picks
  `Isolation::Direct` only when no sandbox probe ran, which is kaniko mode on
  `/`; off `/` the full sandbox runs (ADR-0059, ADR-0060).
- The experiment [measured], 2026-11-03, T2 VM, Ubuntu noble
  `6.8.0-147-generic` and jammy `5.15.0-198-generic`, identical results on
  both. ranikust ran in overlay mode as root with a full sandbox: a non-root
  `USER` `RUN` in its own mount, PID, UTS and IPC namespaces, no user
  namespace, entering its root with `pivot_root`. The account A is a separate
  host account outside the build. The probe is a static C helper that prints
  `getresuid`, set setuid-root in the image, run by A through
  `/proc/<pid>/root` of the `RUN`. (A setuid copy of busybox does not do:
  busybox drops privileges itself.) Host settings: `kernel.yama.ptrace_scope=1`,
  `fs.suid_dumpable=2`, ext4, LSMs `lockdown,capability,landlock,yama,apparmor`.
  The experiment's source is not kept in this repository.
- A same-uid account passes the kernel's check for `/proc/<pid>/root` and
  `/proc/<pid>/environ` [external]: both are governed by
  `PTRACE_MODE_READ_FSCREDS` (`proc_pid_root(5)`, `proc_pid_environ(5)`),
  which allows a caller whose filesystem uid and gid match the target's ids
  when the target is dumpable (`ptrace(2)`, "Ptrace access mode checking").
  The experiment confirms it and shows both ids and the dumpable state matter
  [measured]: A reads `/proc/<pid>/environ` and the `root`, `cwd` and `fd/*`
  targets, and through the root what A's uid may read, only when A's uid and
  primary gid both equal the `RUN`'s and the `RUN` is dumpable. `USER 1234`
  with no passwd entry runs as gid 0, so an account 1234:1234 gets `EACCES`
  on all of it and can only list the descriptor numbers; `USER 1234:1234`
  makes the account 1234:1234 the one that gets in; a different uid is always
  refused. An exec-only (`0711`) binary makes the `RUN` non-dumpable, and then
  nothing is reachable. Root-only files (`0600`) stay unreadable through the
  root. The `/proc/<pid>/fd` listing (names only) needs only a uid match.
- A foreign mount is `nosuid` [measured]: a setuid-root file executed by A
  through `/proc/<pid>/root` of the `RUN` gives `euid=1234`, not 0. The
  `RUN`'s root mount has no `nosuid` flag (`rw,nodev,relatime`), so the cause
  is the foreign mount namespace; the same rule appears with a plain
  `unshare -m` and a tmpfs, without ranikust. Controls: the same file in A's
  own namespace gives `euid=0` with a full `CapEff`, and through
  `/proc/<hostpid>/root` of a host process in A's own mount namespace it also
  gives `euid=0`, so the `/proc` route does not disable setuid; the foreign
  namespace does. Inside the `RUN` setuid works: `euid=0` with `CapBnd`
  `a80425fb`. The kernel source has the same rule [external]: `mnt_may_suid()`
  in `fs/namespace.c` requires the mount to be in the caller's mount namespace
  before honouring setuid, setgid and file capabilities.
- A hardlink out of that root fails [measured]: a hardlink from that path
  into A's namespace fails with `EXDEV` with `fs.protected_hardlinks` at 1 and
  at 0. `EXDEV` comes from the two paths being on different mounts, which
  holds for any two mounts, even within one namespace (`linkat(2)`
  [external]), and the kernel returns it before the `protected_hardlinks`
  check, so that sysctl plays no part.
- `/proc/<pid>/mountinfo` of the `RUN` [measured]: readable by any host user
  when `hidepid` is 0. It shows the state directory and overlay layer paths;
  only the names leak, since the tree is `0700`.

## More Information

Corrects the second Consequence of
[ADR-0047](0047-kaniko-workdir-chain-is-private.md); it supersedes nothing,
and ADR-0047 keeps `accepted`. Related:
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) (partly
supersedes ADR-0047), [ADR-0003](0003-dockerfile-trust-and-default-isolation.md)
(`--userns`), [ADR-0043](0043-direct-run-on-a-workdir-chroots-with-placed-files.md)
(the `Direct` `chroot`). The T2 experiment of 2026-11-03 (Evidence) ran a
non-root `USER` `RUN` in the full sandbox, a setuid-root file in its root and
a same-uid host account executing it through `/proc/<pid>/root`; what it left
unmeasured is listed in the Consequences.
