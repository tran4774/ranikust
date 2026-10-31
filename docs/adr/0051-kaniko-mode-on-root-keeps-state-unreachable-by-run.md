---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0051: Kaniko mode on `/` keeps its state and credentials out of `RUN`'s reach: anonymous files, a non-dumpable process, no ptrace for `RUN`, no surviving processes

## Context and Problem Statement

In a container without extra privileges (Docker's default capabilities,
seccomp and AppArmor, no `CAP_SYS_ADMIN`, no user namespace), kaniko mode
runs `RUN` as root on the same `/` as ranikust, in the same PID namespace.
Hiding the state directory with a mount namespace or an overmount is not
possible there. A `RUN` can read whatever has a path, and, through
`/proc/<pid>/`, another process's descriptors, environment and memory. The
maintainer's goal: a `RUN` cannot read ranikust's state or credentials by
any route; hiding that the directory exists and resisting denial of
service are out of scope. A proof of concept (rootless podman, a default
container) confirmed the core and found two routes it missed.

## Considered Options

- Accept the exposure, as kaniko does with `/kaniko`.
- Keep the state on descriptors only and close `/proc` to `RUN`.

## Decision Outcome

Chosen: state on descriptors only, `/proc` closed to `RUN`, for kaniko mode
on `/` (B2). Overlay mode and kaniko mode on a host keep their persistent
store: there `RUN` has its own namespaces.

- **Anonymous state (B1).** Every state file and blob is an inode without
  a path: `O_TMPFILE | O_RDWR`, mode `0600`, in the state directory, held by
  descriptor; the directory stays empty. Where the filesystem lacks
  `O_TMPFILE` (overlayfs before Linux 6.6 [external, to verify]),
  `memfd_create`, decided after a measurement on kernel 5.15 (B3); never a
  named file unlinked later, which leaves a window. State does not outlive
  the process; cache between builds comes from a remote cache repository
  (Phase 4). One descriptor per blob: `RLIMIT_NOFILE` is raised to its hard
  limit at start (already done), not packed into one file by offset. A
  tree that must outlive the emptying of the root (`COPY --from` files, a
  stage image for `FROM`) is kept as a tar in an anonymous file.
- **Non-dumpable process.** `prctl(PR_SET_DUMPABLE, 0)` first in `main`,
  again after any credential change: `/proc/<ranikust>/{fd,environ,mem,
  maps,root}` need `CAP_SYS_PTRACE`.
- **No ptrace for `RUN`.** In the `RUN` child, before `execve`, every
  capability outside Docker's defaults is dropped from the bounding set
  (the child is single-threaded, so no constraint on ranikust's own
  threads). Where it cannot be dropped (no `CAP_SETPCAP`) and the bounding
  set still holds `CAP_SYS_PTRACE` or `CAP_DAC_READ_SEARCH`, the build
  stops with the reason; no flag lifts it.
- **No surviving process (B6).** ranikust is a subreaper; after each `RUN`
  it kills the `RUN`'s process tree (a cgroup of its own where ranikust may
  make one, else the descendants it tracks, `setsid` ones included) and
  reaps exactly those pids, never `waitpid(-1)`. Helpers of ranikust's own
  (an external `git`) are not touched.
- **Credentials in memory only.** `RUN` gets only the Dockerfile's
  environment; every descriptor is `O_CLOEXEC`; registry credentials are
  read from the environment (`DOCKER_AUTH_CONFIG`) into memory and never
  written to disk. A credential file (`config.json`) is still read; the
  docs say `RUN` can read any file mounted into the container (B7).
- **Routes the proof of concept found (B4, B5).** `/proc/<pid>/cmdline` is
  readable by every process even when not dumpable: a `--build-arg` whose
  name looks like a secret (`TOKEN`, `PASSWORD`, `SECRET`, `KEY`, …) gets a
  warning to pass it as `--secret`. A parent process (a CI shell) that holds
  the credential variables leaks them through its own `environ`: at start,
  ranikust warns when another process holds a variable it reads, and the
  docs advise running ranikust as the container's PID 1.

### Consequences

- Good, because a `RUN` in a default container reads neither the store nor
  ranikust's environment, descriptors or memory, even with `SYS_PTRACE`
  added to the container.
- Good, because no `RUN` process lives on into the walk, the export or the
  push.
- Bad, because kaniko mode on `/` keeps nothing between runs.
- Bad, because the pipeline must pass credentials in the environment and
  run ranikust as PID 1 to close every route; ranikust can only warn.
- Neutral, because files the operator mounts into the container (a
  `config.json`, a service-account token) stay readable by `RUN`.

## Evidence

- Proof of concept, 2026-10-31, rootless podman, `node:22-bookworm-slim`,
  kernel 7.0 [measured]: with `PR_SET_DUMPABLE 0` and an `O_TMPFILE` file,
  a root `RUN` read neither the state nor `/proc/<pid>/{fd,environ}`; it
  read both when the flag was off; `/proc/<pid>/cmdline` stayed readable;
  a parent shell's `environ` leaked the token; with `--cap-add SYS_PTRACE`
  both leaked until the `RUN` child dropped `CAP_SYS_PTRACE` from its
  bounding set. To repeat in the VM under rootful Docker with vectors
  V1–V9 (state, `/proc/*/fd`, `/proc/*/environ`, own environment,
  surviving processes, `unshare -Ur`, cmdline, memory, a parent shell).

## More Information

Built after [ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md)
(Phase 2 plan, `docs/agents/phase-2.md`). Related: the threat model's T7
and T11; ADR-0043 (`Direct` `RUN`).
