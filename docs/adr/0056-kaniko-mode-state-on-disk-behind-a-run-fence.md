---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0056: Kaniko mode on `/` keeps its anonymous state on disk, named only while no `RUN` runs; kept stages are tars

## Context and Problem Statement

ADR-0051 keeps kaniko mode's state on `/` out of a `RUN`'s reach as
anonymous files: `O_TMPFILE` in the state directory, `memfd` where the
filesystem refuses it. A container's overlayfs root refuses `O_TMPFILE`
on kernels 6.8 and 5.15 [measured, VM `secrets` suite], so in the usual
case every pulled blob and encoded layer sat in memory, counted against
the container's memory limit: a large image could be OOM-killed (Q-340).
The kept trees of later-stage reads (`COPY --from`, `FROM <stage>`) were
still directories a `RUN` could read (Q-350). The maintainer asked
(2026-10-31): "Làm 1 và 2 đi. Làm sao tối ưu bộ nhớ nhất và an toàn là
được": as little memory in ranikust as possible, leaving it to `RUN`,
safely. Option 1, streaming layers so nothing proportional to the image is
kept, is ADR-0055; this ADR is option 2.

## Considered Options

- Keep `memfd` and document a volume at the state directory.
- Refuse kaniko mode on `/` without `O_TMPFILE` unless a flag accepts
  memory.
- A file created by name and unlinked at once, which ADR-0051 ruled out
  for the moment a name exists, made safe by creating names only while no
  `RUN` process exists.

## Decision Outcome

Chosen: the third. In kaniko mode on `/`:

- **Order of ways.** An anonymous file is an `O_TMPFILE` inode where the
  state directory's filesystem takes it; else a file created by a random
  name (`.ranikust-anon-` and 128 random bits, `O_CREAT | O_EXCL |
  O_NOFOLLOW | O_CLOEXEC`, `0600`), opened and unlinked at once; `memfd`
  only when no file can be made on disk at all, with a warning.
- **The fence.** A name exists only while no `RUN` process does. The store
  holds a fence (`rnk_store`): `Store::enter_run` counts a `RUN` in once no
  name is on disk and is held by the builder around `Sandbox::run`, which
  returns after the sweep of the `RUN`'s process tree (ADR-0051);
  `Store::named_window` waits until no `RUN` runs. A name asked for from
  inside a `RUN` on the same thread, or a `RUN` while a name outlives ten
  minutes, is a bug in the order of work and fails the build, never a
  race to wait out or a fallback to memory. A pull for a later stage that
  needs a named file during a `RUN` waits for it to end.
- **Crashes.** A crash between create and unlink leaves a name with the
  prefix in the state directory; the next anonymous store opened there
  removes it (files and links unlinked, never followed; directories
  emptied through descriptors on their own mount).
- **Kept stages (Q-350).** A stage kept for a later one is a plain tar in
  an anonymous file owned by the stage (`Store::scratch_file`), written by
  a metadata-only walk of the root and the walk's exporter, with the ignore
  list leaving out the state directory, mounts and the rest; an image read
  by `COPY --from=<image>` stays its blobs. A reader's view is unpacked
  into a directory of the state directory named by the store, inside a
  fence window held until the next instruction of any stage deletes it
  (`StageRootfs::begin_layer`, and `prepare`, `keep`, `release`, the end
  of the build). A stage built `FROM` a kept tar gets it extracted into
  the emptied root, device nodes and FIFOs left out as for any extraction
  into a live root (ADR-0041). On a root other than `/` kept stages stay
  directories beside the root, as before.

### Consequences

- Good, because ranikust's state no longer counts against the container's
  memory: unlinked files are disk with reclaimable page cache. Measured
  in the VM (Docker 29.1.3, cgroup v2, `--memory 256m`): 2 GiB written
  through a named-then-unlinked file on the overlay root kept
  `memory.current` at the limit with `file` ≈ 245 MiB, `shmem` 0, no OOM
  kill, with or without swap, read back intact; the same through `memfd`
  was OOM-killed at about 200–256 MiB.
- Good, because no default needs a flag or a volume, and `RUN` still
  reaches nothing: names never coexist with a `RUN` process, and the
  unlinked files are invisible by name (`ls`, `find -links 0`) and closed
  through `/proc/<ranikust>/fd` (ADR-0051).
- Bad, because the state now writes to the container's writable layer,
  so the host's disk under `/var/lib/docker` takes it until the files are
  closed; a full disk fails the build (then `memfd` is tried, with a
  warning).
- Bad, because a pull overlapping a `RUN` waits for it when it needs a
  named file; kaniko mode on `/` runs stages one at a time anyway.
- Neutral, because a process of the container that is not a `RUN`'s (a CI
  shell, a sidecar) could see a name for the microseconds it exists; such
  processes already hold the credentials ADR-0051 protects.

## Evidence

- The VM PoC above (poc-runner, 2026-11-01, kernel 6.8): reclaimable page
  cache for unlinked overlay files, OOM for `memfd`; `O_TMPFILE` refused
  on the overlay root, accepted on tmpfs and ext4 volumes; after the
  unlink, no name; with `PR_SET_DUMPABLE 0`, `EACCES` on
  `/proc/<pid>/fd/N` for a root process without `CAP_SYS_PTRACE`.
- VM `secrets` suite: the `memlimit` scenario builds a 512 MiB layer under
  `--memory 256m --memory-swap 256m`.
- Not measured: kernel 5.15, overlay on xfs or btrfs, write time under
  reclaim.

## More Information

Answers Q-340 and Q-350. Implements ADR-0051's anonymous state without
its memory cost; ADR-0055 (streaming) removes most of what is stored at
all. Design: `design/kaniko-mode.md#invariants`.
