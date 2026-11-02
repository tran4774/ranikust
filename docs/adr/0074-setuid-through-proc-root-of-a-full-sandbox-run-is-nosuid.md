---
status: proposed
date: 2026-11-02
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
with `EXDEV`; the rest of ADR-0047 stands.

- **What stays true.** While the `RUN` lives as a non-root `USER`, a distinct
  host account with the same uid can still read its `/proc/<pid>/environ` and
  reach its root through `/proc/<pid>/root`, with that uid's file
  permissions, until `--userns` (Phase 3, [ADR-0003](0003-dockerfile-trust-and-default-isolation.md)).
  The threat model now lists this as an accepted risk
  ([section 6](../security/threat-model.md#6-accepted-risks)).
- **Where ADR-0047's sentence was true.** Only for a `RUN` in the host's own
  mount namespace (by the same kernel rule [inferred]): the `Direct` `chroot`
  of a host `--workdir` that ADR-0047 first described. [ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md)
  removed that workdir, and the binary runs a `Direct` `RUN` on `/` of a
  container only, so no mode has that case today.
- **The private chain stands.** Its purpose, that no other user reaches the
  image's setuid files by path or hardlinks one out, is unchanged; ADR-0050
  moved the chain to the state directory.

### Consequences

- Good, because the threat model no longer tells a reader that a same-uid
  account can turn an image's setuid file into privilege through
  `/proc/<pid>/root`.
- Bad, because the kernel rule is [inferred], not measured here: until a VM
  experiment shows it, this ADR stays `proposed`.
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
- A same-uid account passes the kernel's check for `/proc/<pid>/root` and
  `/proc/<pid>/environ` [external]: both are governed by
  `PTRACE_MODE_READ_FSCREDS` (`proc_pid_root(5)`, `proc_pid_environ(5)`),
  which allows a caller whose filesystem uid and gid match the target's ids
  when the target is dumpable (`ptrace(2)`, "Ptrace access mode checking").
- A foreign mount is `nosuid`, and a link across mounts fails [inferred]:
  from `mnt_may_suid()` in the kernel's `fs/namespace.c`, which requires the
  mount to be in the caller's mount namespace before honouring setuid, setgid
  and file capabilities, and from `linkat(2)`'s `EXDEV` for two mounts. Not in
  this repository and not yet measured.

## More Information

Corrects the second Consequence of
[ADR-0047](0047-kaniko-workdir-chain-is-private.md); it supersedes nothing,
and ADR-0047 keeps `accepted`. Related:
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) (partly
supersedes ADR-0047), [ADR-0003](0003-dockerfile-trust-and-default-isolation.md)
(`--userns`), [ADR-0043](0043-direct-run-on-a-workdir-chroots-with-placed-files.md)
(the `Direct` `chroot`). Revisit once the T2 experiment is run: a non-root
`USER` `RUN` in the full sandbox, a setuid-root file in its root, and a
same-uid host account executing it through `/proc/<pid>/root`.
