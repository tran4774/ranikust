---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0132: The `/proc` rbind is bound read-only, so a `RUN` writes nothing under it

## Context and Problem Statement

ADR-0126 lets an operator allow, with `--allow proc.rbind`, an rbind of the
container's `/proc` for each `RUN` of a rootless build that cannot mount a new
`proc`. The rbind is bound `nosuid`, `nodev` and `noexec`, but not read-only; the
rbind of `/sys` in the same place already is. ADR-0130 measured what such a `RUN`
reaches, and found reads: command lines, status, mount tables and network tables;
the ptrace-gated files are refused across the user-namespace boundary. It tested
no write. A review of the rbind then found that some `/proc/<pid>` files are
checked only by file ownership against the kernel uid, not by ptrace access, so a
`RUN` could change owner-writable attributes of the processes of its kernel uid
outside the build, such as the OOM score and the scheduling priority. The rbind
has not shipped in a release, and the fix goes into the same unreleased pull
request. The question put to the maintainer in the question window, which has no
`question` issue: bind the rbind read-only, or keep it read-write and accept the
residual?

## Considered Options

The options are those of the question as put to the maintainer.

- Bind the rbind read-only (recommended): add `MOUNT_ATTR_RDONLY` to the rbind's
  flags and record this ADR. Every write to `/proc` from outside the build is
  refused. Cost: a `RUN` cannot write `/proc/self/*` (its own `oom_score_adj`, the
  `uid_map` of a nested user namespace and the like), which a build rarely needs,
  and only the opt-in rbind is affected.
- Keep it read-write and accept the residual: the writes would be named in the
  threat model's section 6 and in the flag's warning, and tracked in a draft
  advisory rated Medium (4.2). It is closer to Docker for `/proc/self`, where
  those files stay writable.

## Decision Outcome

The rbind of the container's `/proc` that `--allow proc.rbind` gives a `RUN` is
bound read-only, with `MOUNT_ATTR_RDONLY` added to the `nosuid`, `nodev` and
`noexec` of ADR-0126, as the `/sys` rbind is.

The maintainer's answer (2026-11-07: "Rbind read-only + ADR (khuyến nghị)", "Rbind
read-only + ADR (recommended)") stood for the first option: add
`MOUNT_ATTR_RDONLY`, record this ADR, and block every write to `/proc` from
outside the build, at the cost that a `RUN` cannot write `/proc/self/*`.

- **What fails.** Under the rbind a `RUN`'s writes to `/proc` fail with `EROFS`:
  - its own `oom_score_adj`;
  - `attr/*` (`runcon`, `aa-exec`);
  - `task/<tid>/comm` of another thread;
  - `uid_map`, `gid_map` and `setgroups` of a nested user namespace, which matters
    only under `--seccomp=unconfined`, since the default filter refuses new user
    namespaces.
- **What does not change.**
  - `/proc/sys` was already read-only.
  - Writes through magic links (`/dev/stderr`, `/proc/self/fd/N`) work as before:
    the check falls on the mount of the link's target, not on `/proc`.
  - The sandbox writes nothing under the rbind; its `uid_map` writes go through the
    spawner's own `/proc`.
  - A new `proc`, the normal case, is bound as before.
- **Scope.** Only the rbind a rootless build gets with `--allow proc.rbind`. The
  residual of ADR-0130 stays reads only, and now holds for writes too.
- **Not decided here.** Whether the exposure closes, and the opt-in's default
  changes, once a `RUN` runs under a uid other than that of the container's
  processes ([#34](https://github.com/tran4774/ranikust/issues/34),
  [#83](https://github.com/tran4774/ranikust/issues/83)); ADR-0126 and ADR-0130 left
  it open and this ADR does too.

### Consequences

- Good, because a `RUN` can change no attribute of a process outside the build
  through `/proc`, whatever the file's owner check: the residual is the reads
  ADR-0130 measured, and no write.
- Good, because the rbind of `/proc` and that of `/sys` now carry the same
  read-only rule, and no per-file list of owner-writable entries has to be kept
  current as the kernel adds one.
- Good, because no advisory is needed: the rbind has not shipped in a release, and
  the flags are fixed in the pull request that builds it.
- Bad, because a `RUN` in a build with `--allow proc.rbind` cannot write
  `/proc/self/*`: a tool that lowers its own `oom_score_adj`, sets an LSM context
  through `attr/*`, or maps ids in a nested user namespace fails with `EROFS`
  where it works in Docker, so the flag's help and the book must say so.
- Neutral, because a new `proc` is unchanged, and so is any build that does not
  pass the flag.

## Evidence

- **[code]** `rnk-sandbox` on the unmerged branch `feat/sandbox-rootless-run`
  (`crates/rnk-sandbox/src/child.rs`, `mount_one`): the `/proc` rbind is made with
  `MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC`; the `/sys` rbind
  adds `MOUNT_ATTR_RDONLY`.
- **[inferred]** From a review of the rbind, not from a test: some `/proc/<pid>`
  files are checked by file ownership against the kernel uid and not by ptrace
  access, so the user-namespace refusal that ADR-0130 measured for the ptrace-gated
  files does not cover them. The details of the review are not recorded here.
- **[inferred]** That a read-only bind makes each write above fail with `EROFS`,
  and leaves writes through magic links alone (the check falls on the target's
  mount), from how the mount flag is checked; nothing measures them yet. The tests
  belong to the pull request that makes the change.
- **[inferred]** That Docker leaves `/proc/self/*` writable and makes `/proc/sys`
  and a few other paths read-only: the threat model's T17 lists those read-only
  paths from moby's defaults, and says nothing about `/proc/self`.

## More Information

Refines ADR-0126 (the rbind's flags: read-only is added to its `nosuid`, `nodev`
and `noexec`) and ADR-0130 (its residual of reads holds for writes too). Neither
is rewritten, and both stay `accepted`. ADR-0126 and ADR-0130 are on the unmerged
branch `feat/sandbox-rootless-run`, so they are named here, not linked.

Answers a question put to the maintainer in the question window; it has no
`question` issue, so this pull request closes nothing. The code and the docs
that say what an rbind `RUN` can do (the threat model's section 6 entry and its
T12 row, `docs/design/sandbox.md`, the `rnk-sandbox` card, the doc comment of
`Fallbacks::allow_proc_rbind`) change in the pull request of
[#78](https://github.com/tran4774/ranikust/issues/78), which this branch is merged
into; the flag's help and warning, with the `## Changelog` entry, come with the
pull request that builds the flag
([#79](https://github.com/tran4774/ranikust/issues/79)). Revisit only if a measurement shows that a build
needs a write under `/proc/self`.
