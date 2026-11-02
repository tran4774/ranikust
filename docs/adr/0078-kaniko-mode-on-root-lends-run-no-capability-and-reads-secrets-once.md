---
status: accepted
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0078: Kaniko mode on `/` lends a `RUN` no capability, and ranikust reads its configuration and secrets once

## Context and Problem Statement

In kaniko mode on a container's `/` (`Isolation::Direct`), ADR-0051 drops
every capability outside Docker's defaults from the `RUN` child only. It
says this puts "no constraint on ranikust's own threads". So ranikust
itself keeps them for the whole build. One example is `CAP_SYS_PTRACE` in
a container started with `--cap-add SYS_PTRACE`, which ADR-0051 supports.

After the `RUN`s, ranikust opens operator paths by name: the outputs, the
metrics report, and every `--secret src=` file. It re-opens the secret
file for each `RUN` that mounts it. Those paths lie on volumes a root
`RUN` on the same `/` can write. A `RUN` can therefore plant a symlink
there and make ranikust follow it with authority the `RUN` was denied:

- the root of a peer process (`/proc/<pid>/root/...`) that the `RUN`
  cannot reach by itself;
- ranikust's own `/proc/self/...`, which is closed to the `RUN` because
  ranikust is not dumpable.

For a secret, the bytes then reach the next `RUN` as its variable. The
question: how does ranikust stop lending its authority to a `RUN`?

## Considered Options

- **(A)** ranikust gives up every capability its `RUN` lacks before the
  first `RUN`, and reads its configuration and secrets once, at start-up,
  keeping them in memory.
- **(B)** Pin every operator path by descriptor at start-up and write
  through it later. This is more code, and every output's parent must
  exist at the start. It also leaves ranikust's extra capabilities live
  for any other path it opens later.
- **(C)** Keep it as is, and document it as an accepted risk.

## Decision Outcome

Chosen: (A), by the maintainer on 2026-11-02 ("3: A"): ranikust reads all
its configuration and secrets once, at start-up, and keeps them in memory
so that it never reads them again.

**No capability lent (kaniko mode on `/` only).** At set-up, before the
state directory, the store and the spawner thread, ranikust calls
`rnk_sandbox::trim_to_run_capabilities`. That call:

- refuses unless the process is single-threaded, because capabilities
  are per thread: every thread and child made later inherits the sets;
- cuts the bounding set to Docker's defaults where `CAP_SETPCAP` allows
  it; without `CAP_SETPCAP` the build is refused, as ADR-0051 already
  does for the `RUN`;
- clears the ambient and inheritable sets, and keeps the permitted and
  effective sets to the permitted capabilities among Docker's defaults;
- sets `PR_SET_DUMPABLE` to 0 again, as ADR-0051 asks after any
  credential change;
- reads the sets back, and fails the build if any capability outside the
  defaults is left.

The `RUN` child's own trim stays as it is. Overlay mode and kaniko mode
with per-stage directories keep their capabilities, because they mount.
Their `RUN` runs in its own mount namespace and never reaches ranikust's
paths.

**Read once (every mode).** Every `--secret` source, file or variable, is
read into memory while the build is prepared, before the first
instruction. No path is opened again.

- Each secret is one copy, shared by every `RUN` that mounts it.
- `Debug` never shows the bytes.
- The copy is overwritten with zeros when the build drops it. Copies
  handed to a `RUN` (its variable, its tmpfs file) are not wiped
  (Q-444).

The same rule, already true in the code, now holds by decision: the
Dockerfile, `.dockerignore`, the registry credentials (`DOCKER_AUTH_CONFIG`,
`config.json`), the host CA bundle and the `--registry-certificate` files
are read once, before the first instruction.

This decision does not cover:

- the build context's files, which `COPY` reads when it runs through the
  descriptors pinned by the context walk;
- the resolver files `/etc/hosts` and `/etc/resolv.conf`, which the C
  library reads at each lookup. ADR-0051 already sends credentials only
  over verified TLS because of them (Q-446).

**Secret files on `/`.** A `--secret src=` file on `/` stays readable by
every `RUN` there (threat-model section 6). ranikust now warns about each
one in kaniko mode on `/`. Whether to refuse such files is Q-445.

### Consequences

- Good, because after a `RUN`, ranikust does on `/` only what that `RUN`
  could do itself. A link planted at an output or `--secret` path borrows
  no capability.
- Good, because a secret swapped or repointed during the build changes
  nothing a later `RUN` gets, and the file is read once rather than once
  per `RUN`.
- Bad, because in kaniko mode on `/` a container that grants ranikust
  extra capabilities no longer lets ranikust use them. None is needed
  there: no mount, and deletion, the sweep and the outputs need only the
  defaults. One effect is that an image's `security.*` attributes other
  than `security.capability`, which need `CAP_SYS_ADMIN`, are handled as
  in a default container.
- Bad, because a secret file changed during the build is not seen. BuildKit
  reads a secret again at each use.
- Neutral, because secret bytes stay in ranikust's memory for the whole
  build. The process is not dumpable and leaves no core file
  (ADR-0051).
- Neutral, because a symlink at an output path to ranikust's own
  `/proc/self/fd/<n>` can still make it write into one of its own files.
  That gives a `RUN` nothing beyond its own build's outputs, which it can
  already write. Opening operator paths without following links closes
  it.

## Evidence

- VM suite `secrets`, 2026-11-02, rootful Docker, Ubuntu 24.04 (kernel
  6.8) [measured]:
  - **V11:** compares ranikust's capability sets with the `RUN`'s.
    Before this change, in the `--cap-add SYS_PTRACE` scenario, ranikust
    held `CapPrm`, `CapEff` and `CapBnd` `a80c25fb` against the `RUN`'s
    `a80425fb`, the difference being `CAP_SYS_PTRACE`. After it, ranikust
    holds nothing beyond the `RUN`'s in any scenario.
  - **V12:** an earlier `RUN` swapped a `--secret` file for a symlink to
    `/proc/self/comm`. Before this change, the later `RUN`'s variable held
    `ranikust`, ranikust's own name, in all three scenarios. After it,
    the variable held the original bytes.
  - V1 to V10 are unchanged.
- T0 `a_file_secret_swapped_after_the_start_changes_nothing`
  (`rnk-builder`) [measured]: before this change, a later `RUN` got the
  planted file's bytes.
- `ptrace(2)`, "Ptrace access mode checking" [external]: `/proc/<pid>/root`
  and `/proc/<pid>/fd` need `PTRACE_MODE_READ_FSCREDS`, which
  `CAP_SYS_PTRACE` passes. A process's own `/proc/self` is exempt.

## More Information

Refines [ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md):
its "no constraint on ranikust's own threads" no longer holds on `/`. Its
"credentials in memory only" now covers every `--secret`. The design is in
[kaniko-mode](../design/kaniko-mode.md#invariants), and
the threat is T21 of the [threat model](../security/threat-model.md).
Open: Q-444 (wiping every copy of a secret), Q-445 (refusing `--secret
src=` files on `/`), Q-446 (the resolver files).
