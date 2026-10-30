# Phase 2: kaniko mode

**TL;DR:** Build the same Dockerfiles where ranikust is root but may not
mount (default Docker, ordinary pods): the workdir is the root, each layer
comes from a parallel walk and hash, the root is emptied safely between
stages, and stages run one at a time unless per-stage directories are
possible or `--parallel-stages` asks for them. Measured against kaniko on
W1–W3. Its open questions are settled (`ADR-0038`, `ADR-0039`) except the
`Snapshotter` signature, revisited at task 6 (Q-024).

Source: survey 04 §3 (5–6 weeks for one person `[inferred]`),
[design/kaniko-mode.md](../design/kaniko-mode.md), experiments 09, 12, 12a
and 13–16 (`docs/research/experiments/`). Tick items here as they land.

## Scope

Status `[ ]` open, `[~]` in progress, `[x]` done; owner = the crate holding
the code.

| | Item | Owner |
|---|---|---|
| [x] | Benchmark of the snapshot hash (`xxhash-rust` xxh3, `highway`) and the parallel walk (`jwalk`, `rayon` + `getdents64`) on a node rootfs; the picks recorded in an ADR (`tests/bench/walk-hash`, `ADR-0039`: `rayon` + `getdents64`, xxh3-128, 8 workers by default, `--snapshot-workers`) | `rnk-snapshot`; bench |
| [x] | Workdir checks before anything is written: canonicalized, `/` and the 30 system directories and their ancestors refused by `(dev, ino)`, opened with `RESOLVE_NO_SYMLINKS`, `flock(LOCK_EX \| LOCK_NB)` held for the build, the `.ranikust-root` marker (a workdir of only mount points is refused); `RLIMIT_NOFILE` raised to its hard limit (`ADR-0038`, Q-011) | `ranikust`, with `rnk-rootfs` primitives |
| [x] | The kaniko-mode state directory: `/ranikust/state` when the root is `/`, otherwise `<cwd>/.ranikust`, the directory ranikust was started in; on the ignore list whenever it lies inside the root (`ADR-0038`, Q-036) | `ranikust`, `rnk-store` |
| [x] | `--mode=auto` picks kaniko mode when root may not mount, and says why; the probe's per-stage outcome (can mount, overlay unusable) | `ranikust` |
| [ ] | Ignore list: mount points under the root from `/proc/self/mountinfo`, `--ignore-path`, `/proc`, `/sys`, `/dev`, all of `/ranikust`, the state directory, `/etc/mtab`, `/tmp/apt-key-gpghome`, and `/var/run` while `--ignore-var-run` is true (`ADR-0038`, Q-035) | `rnk-snapshot`, on `rnk-rootfs::IgnoreList` |
| [ ] | Base-layer extraction into a live root: whiteouts applied (`Whiteouts::Apply`), ignored paths never written, mount points kept | `rnk-rootfs` |
| [x] | Parallel walk relative to each parent fd (`openat2` `RESOLVE_BENEATH \| NO_SYMLINKS \| NO_MAGICLINKS \| NO_XDEV`, `statx` per entry, mount-id guard), 8 workers by default, `--snapshot-workers` (`ADR-0039`); content and xattr hash (`*.overlay.*` excluded); stat cache with the 2 s racy window; the diff computed during the walk over a path arena (kaniko's `LayeredMap`); `--snapshot-mode=full\|redo\|time` | `rnk-snapshot` |
| [ ] | Walk diff → OCI layer: one whiteout at the topmost deleted path, parent headers, a hardlink into a lower layer written as a regular file, an empty layer for a `RUN` that changes nothing | `rnk-snapshot` → `rnk-layer` |
| [ ] | `StageRootfs`/`Snapshotter` for kaniko mode (with one policy, as kaniko, for an entry that vanishes or is swapped during the walk: today a directory fails the walk with `Error::Changed` while a file is skipped; run the T2 device-swap test of the walk first), on the shared `/` and on a per-stage directory; revisit the interim signature ([open: Q-024]) | `rnk-snapshot` |
| [ ] | Safe deletion between stages and on release, with the ignore list (mechanism done in `rnk_rootfs::delete_contents`), from `Workdir::root()`, never a reopened path; `Workdir::mark` before the first write; a state directory inside the workdir (`<cwd>/.ranikust` with the cwd in the workdir) neither counts against the marker check nor is deleted; kaniko mode requires root | `rnk-snapshot` |
| [~] | Sequential flow: kaniko's stage squashing (`squash_for_shared_root`); what later stages need (`COPY --from` files, a stage image built `FROM`) kept in the state directory before the root is emptied | `rnk-dockerfile`, `rnk-builder` |
| [~] | Scheduler: a stage on the shared `/` runs alone, `--jobs` ignored there with one warning; per-stage directories scheduled as overlay stages | `rnk-sched` |
| [~] | Lazy snapshots: a stage is walked only when its layers are needed, decided from the graph before the build (`ADR-0038`, Q-034) | `rnk-sched`, `rnk-builder` |
| [ ] | `RUN` on the shared root with `Isolation::Direct`, with the `RLIMIT_NOFILE` ranikust started with (task 1 raises its own); placed files (`hosts`, `resolv.conf`, stubs, `/dev` nodes) journaled, removed and their parent's mtime restored before the walk | `rnk-sandbox`, `rnk-snapshot` |
| [ ] | Per-stage directories `builds/<id>/stages/<n>/rootfs` in the state directory, by extraction or reflink copy, never hardlinks (`ADR-0017`); the full sandbox where mounting is possible | `rnk-snapshot`, `rnk-store`, `rnk-sandbox` |
| [ ] | `--parallel-stages` (the final name, `ADR-0038`, Q-019): `Isolation::BareChroot`, `/dev` by `mknod`, child subreaper, the start-up log line; fake `/proc` with the fourteen files per cgroup version (`ADR-0038`, Q-037); `JAVA_TOOL_OPTIONS` under cgroup limits (`ADR-0019`); the Java warnings and the hints after a failed `RUN` | `rnk-sandbox`, `ranikust` |
| [~] | kaniko flags refused until Phase 2 (`ADR-0036`): `--ignore-path`, `--ignore-var-run`, `--whitelist-var-run`, `--single-snapshot`, `--materialize`, `--snapshot-mode` taking effect; the ignored class revisited for kaniko mode | `ranikust` |
| [ ] | Metrics: `rnk_snapshot.baseline`, `.walk`, `.resolve`, `.syncfs` and the deletion spans ([design/metrics.md](../design/metrics.md#rnk-snapshot)) | each crate |
| [ ] | T2: the VM runs kaniko mode as real root on a host workdir and inside a default `docker run` (no extra capability); deletion tests D1–D3 of experiment 09; the `e2e` suite gets a kaniko-mode leg | tests |

## Task order

Three tracks after task 0: set-up (1, 2), the snapshot (3–6) and execution
(7–11). The snapshot track is the long one.

| # | Task | Needs | Blocked by |
|---|---|---|---|
| 0 | Hash and walk benchmark; ADR with the picks (done, `ADR-0039`) | | |
| 1 | Workdir checks, lock, marker, `RLIMIT_NOFILE` (`ranikust`, `rnk-rootfs`) (done) | | |
| 2 | State directory default, `--mode=auto` picking kaniko mode, the refused flags accepted (done, `ADR-0040`) | 1 | |
| 3 | Ignore list from mountinfo and the fixed paths; extraction into a live root | 1 | |
| 4 | Parallel walk, hash, stat cache, diff during the walk (done; switches to `IgnoreList::covers` when task 3 lands) | 0, 3 | |
| 5 | Walk diff → layer exporter | 4 | |
| 6 | `KanikoRoot` (`StageRootfs`, `Snapshotter`) on `/` and per-stage directories; safe deletion between stages | 3, 4, 5 | Q-024 revisited here |
| 7 | `RUN` with `Isolation::Direct` on the shared root; placed files and mtime restore | 6 | |
| 8 | Sequential flow: squashing, kept stage outputs, shared-`/` exclusivity in `rnk-sched`, lazy snapshots | 6, 7 | |
| 9 | Per-stage directories with the full sandbox | 6, 8 | |
| 10 | `--parallel-stages`: bare `chroot`, `/dev`, subreaper, fake `/proc`, Java | 9 | |
| 11 | CLI end to end in the VM, the e2e kaniko-mode leg, acceptance runs | all | |

Run in parallel from 2026-10-30, each on its own branch and worktree,
because these parts need only what is already built: task 2; task 3; task
4 against the existing `rnk_rootfs::IgnoreList`, so it does not wait for
task 3's list; the sandbox half of task 7 (`Isolation::Direct`, the placed
files' journal and mtime restore), testable on any root without
`KanikoRoot`; and the logic half of task 8 (squashing, the lazy-snapshot
rule from the stage graph, the shared-`/` exclusivity in `rnk-sched`).
Task 5 waits for task 4's diff type (`rnk_snapshot::WalkDiff`); task 6 joins 3, 4 and 5, and wires
the halves of 7 and 8 in.

Every task instruments its crate as its card's `## Metrics` and
[design/metrics.md](../design/metrics.md#spans-per-crate) list, tested
with `rnk_metrics::Capture`; a task is not done without it. Tasks 1, 3, 6,
7, 9 and 10 touch `rnk-rootfs`, `rnk-snapshot`, `rnk-sandbox` or
`rnk-store`: plan mode first and the `host-safety` subagent on the diff.

## Acceptance criteria

- [ ] Builds W1, W2 cold and W3 in sequential kaniko mode inside a default
      `docker run` (root, no `CAP_SYS_ADMIN`) in the VM, and as root on a
      host `--workdir`.
- [ ] Flattened content and config compared with kaniko's on W1–W3: only
      `ADR-0012` differences and the known non-deterministic files.
- [ ] Time per build and per snapshot phase compared with kaniko's metrics
      report: the walk and hash of every step take less than kaniko's
      `snapshot.walk` + `snapshot.hash` (3.18 s on W1, `exp-07`), and no
      build is slower than kaniko's.
- [ ] No mount point (`/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname`,
      volumes), no file of the state directory and no placed file reaches a
      layer; `/etc` is not in a layer only because of a placed file.
- [ ] Host safety: `/`, every system directory and their ancestors refused
      by `(dev, ino)`; a non-empty workdir without the marker refused; a
      second build on the same workdir refused by the lock; deletion keeps
      a same-filesystem bind mount (D1–D3 of experiment 09). T0 and T2.
- [ ] `--reproducible`: two builds at different times give one digest; in
      per-stage mode the digest does not depend on `--jobs`.
- [ ] Per-stage directories (sandboxed) and `--parallel-stages` build W3
      faster than sequential (experiment 12: −41 to −44 %), with peak memory
      recorded; no fake `/proc` file reaches a layer; `next build` passes
      on the bare `chroot` path.
- [ ] The kaniko-mode flags of the scope are tested for their meaning, and
      [kaniko-map.md](kaniko-map.md#flags) and the book are updated.
- [ ] aarch64 T0 green in CI, as in Phase 1.

## Carried over from Phase 1

Not blocking Phase 2; each lands when its owner is next touched:

- config goldens against kaniko and BuildKit (`rnk-builder`);
- the W3 digest compared by content without the workload's `du` output
  ([Q-187](../open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output));
- W1 and W3 metrics reports compared with kaniko's by the bench harness;
- T2 on cgroup v1 and kernels before 5.11, and a CI job with KVM.

## Deferred

As in [phase-1.md](phase-1.md#deferred): rootless overlay and the full
probe (Phase 3), the cache (Phase 4), the remaining instructions and
contexts (Phase 5), hardening (Phase 6).

## Open questions

- **Q-024 `Snapshotter` signature.** Revisited at task 6:
  [Q-024](../open-questions.md#q-024-what-is-the-snapshotter-signature).

Settled for Phase 2 by the maintainer (2026-10-30) in
[ADR-0038](../adr/0038-kaniko-mode-settings-for-phase-2.md): Q-011, Q-019,
Q-034, Q-035, Q-036, Q-037; in
[ADR-0039](../adr/0039-kaniko-mode-walk-and-hash-crates.md): Q-038. Also measured during the phase: the kaniko-mode
items of
[Q-033](../open-questions.md#q-033-which-performance-conditions-are-still-unmeasured)
(cold page cache, slow disks, deleting very large roots).
