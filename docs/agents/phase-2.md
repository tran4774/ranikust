# Phase 2: kaniko mode

**TL;DR:** Build the same Dockerfiles where ranikust is root but may not
mount (default Docker, ordinary pods): the workdir is the root, each layer
comes from a parallel walk and hash, the root is emptied safely between
stages, and stages run one at a time unless per-stage directories are
possible or `--parallel-stages` asks for them. Measured against kaniko on
W1–W3. Its open questions are settled (`ADR-0038`, `ADR-0039`) except the
`Snapshotter` signature, revisited at task 6 (Q-024). **Done** (every
acceptance criterion ticked, 2026-11-03, after the refactor, the security
audit in [`docs/security/audit/phase-1-2/`](../security/audit/README.md)
and its hardening pass); the next phase's plan is not written yet
([roadmap](../../README.md#roadmap)). What Phase 2 leaves to later phases
is under [Carried over to later phases](#carried-over-to-later-phases).

Source: survey 04 §3 (5–6 weeks for one person `[inferred]`),
[design/kaniko-mode.md](../design/kaniko-mode.md), experiments 09, 12, 12a
and 13–16 (`docs/research/experiments/`). Tick items here as they land.

## Scope

Status `[ ]` open, `[~]` in progress, `[x]` done; owner = the crate holding
the code.

| | Item | Owner |
|---|---|---|
| [x] | Benchmark of the snapshot hash (`xxhash-rust` xxh3, `highway`) and the parallel walk (`jwalk`, `rayon` + `getdents64`) on a node rootfs; the picks recorded in an ADR (`tests/bench/walk-hash`, `ADR-0039`: `rayon` + `getdents64`, xxh3-128, 8 workers by default, `--snapshot-workers`) | `rnk-snapshot`; bench |
| [x] | (Superseded by `ADR-0050`: no `--workdir`, state directory `/var/lib/ranikust`.) Workdir checks before anything is written: canonicalized, `/` and the 30 system directories and their ancestors refused by `(dev, ino)`, opened with `RESOLVE_NO_SYMLINKS`, `flock(LOCK_EX \| LOCK_NB)` held for the build, the `.ranikust-root` marker (a workdir of only mount points is refused); `RLIMIT_NOFILE` raised to its hard limit (`ADR-0038`, Q-011) | `ranikust`, with `rnk-rootfs` primitives |
| [x] | (Superseded by `ADR-0050`: no `--workdir`, state directory `/var/lib/ranikust`.) The kaniko-mode state directory: `/ranikust/state` when the root is `/`, otherwise `<cwd>/.ranikust`, the directory ranikust was started in; on the ignore list whenever it lies inside the root (`ADR-0038`, Q-036) | `ranikust`, `rnk-store` |
| [x] | `--mode=auto` picks kaniko mode when root may not mount, and says why; the probe's per-stage outcome (can mount, overlay unusable) | `ranikust` |
| [x] | Ignore list: mount points under the root from `/proc/self/mountinfo`, `--ignore-path`, `/proc`, `/sys`, `/dev`, all of `/ranikust`, the state directory, `/etc/mtab`, `/tmp/apt-key-gpghome`, and `/var/run` while `--ignore-var-run` is true (`ADR-0038`, Q-035) | `rnk-snapshot`, on `rnk-rootfs::IgnoreList` |
| [x] | Base-layer extraction into a live root: whiteouts applied (`Whiteouts::Apply`), ignored paths never written, mount points kept | `rnk-rootfs` |
| [x] | Parallel walk relative to each parent fd (`openat2` `RESOLVE_BENEATH \| NO_SYMLINKS \| NO_MAGICLINKS \| NO_XDEV`, `statx` per entry, mount-id guard), 8 workers by default, `--snapshot-workers` (`ADR-0039`); content and xattr hash (`*.overlay.*` excluded); stat cache with the 2 s racy window; the diff computed during the walk over a path arena (kaniko's `LayeredMap`); `--snapshot-mode=full\|redo\|time` | `rnk-snapshot` |
| [x] | Walk diff → OCI layer: one whiteout at the topmost deleted path, parent headers, a hardlink into a lower layer written as a regular file, an empty layer for a `RUN` that changes nothing | `rnk-snapshot` → `rnk-layer` |
| [x] | `StageRootfs`/`Snapshotter` for kaniko mode (a layer exported to a temporary blob, discarded when `WalkDiff::export` fails part-way; with one policy, as kaniko, for an entry that vanishes or is swapped during the walk: today a directory fails the walk with `Error::Changed` while a file is skipped; run the T2 device-swap test of the walk first), on the shared `/` and on a per-stage directory; signature: `ADR-0046` | `rnk-snapshot` |
| [x] | Safe deletion between stages and on release, with the ignore list (mechanism done in `rnk_rootfs::delete_contents`), from `Workdir::root()`, never a reopened path; `Workdir::mark` before the first write; a state directory inside the workdir (`<cwd>/.ranikust` with the cwd in the workdir) neither counts against the marker check nor is deleted; kaniko mode requires root | `rnk-snapshot` |
| [x] | ranikust picks the build root (`ADR-0050`): no `--workdir`, no `--force`; state directory `/var/lib/ranikust` (`--state-dir`), checked as the private directory of every root, its parents root's; kaniko mode on `/` in a container, else in `<state>/builds/<id>/rootfs`; overlay mode refuses a state directory on overlayfs; kaniko's old spellings refused (`ADR-0052` C4) | `ranikust`, `rnk-snapshot` |
| [x] | Kaniko mode on `/` keeps state and credentials out of `RUN`'s reach (`ADR-0051`, `ADR-0056`): anonymous `O_TMPFILE` state (`memfd_create` fallback after a kernel 5.15 measurement), `PR_SET_DUMPABLE 0`, the bounding set cut to Docker's in the `RUN` child (stop if `CAP_SYS_PTRACE`/`CAP_DAC_READ_SEARCH` stays), a subreaper that kills only the `RUN`'s process tree after each `RUN`, the cmdline and parent-environment warnings; VM test with rootful Docker, vectors V1–V9, also with `--cap-add SYS_PTRACE` | `ranikust`, `rnk-sandbox`, `rnk-store` |
| [x] | (done; audited 2026-11-01: squash.rs, Kept::Tar on `/`, ADR-0052 D2 checks, each with tests) Sequential flow: kaniko's stage squashing (`squash_for_shared_root`); what later stages need (`COPY --from` files, a stage image built `FROM`) kept before the root is emptied, as tars in anonymous files on `/` (`ADR-0051`); `COPY` with an empty source or a bare unknown `--from` name refused (`ADR-0052` D2) | `rnk-dockerfile`, `rnk-builder` |
| [x] | (done; audited 2026-11-01; on `/` `--jobs` still bounds uploads, the warning says so) Scheduler: a stage on the shared `/` runs alone, one warning about `--jobs` there; per-stage directories scheduled as overlay stages | `rnk-sched` |
| [x] | (done; audited 2026-11-01: `layers_needed` + `LayerDemand`, ADR-0044, tests) Lazy snapshots: a stage is walked only when its layers are needed, decided from the graph before the build (`ADR-0038`, Q-034) | `rnk-sched`, `rnk-builder` |
| [x] | `RUN` on the shared root with `Isolation::Direct` (done 2026-11-01: heredoc `#!` from a sealed memfd, `RUN --mount` checked before any secret read, a cache mount left out with a warning, a secret file and `--network=none` refused, every `RUN`'s stdio relayed, `ADR-0057`; D1 marker made at the first base on `/`, D4, empty layers dropped, `ADR-0058`; follow-ups: Q-383 with Phase 5, the Phase 4 cache key telling a dropped cache mount (Q-041), T2 on kernel 5.15 for the memfd fallbacks, a real volume on `/ranikust` at T1; sandbox half: `ADR-0043`; before wiring it on a workdir, run its T2 tests and give a clear error where `CAP_SETPCAP` is missing for the bounding-set drop), with the `RLIMIT_NOFILE` ranikust started with (task 1 raises its own); placed files (`hosts`, `resolv.conf`, stubs, `/dev` nodes) journaled, removed and their parent's mtime restored before the walk; `/` emptied before the first base when a previous ranikust build left its marker under `/ranikust` (`ADR-0052` D1); `/var/run` ignored only when it is a mount point (D4); `COPY --chmod` on directories documented (D3) | `rnk-sandbox`, `rnk-snapshot` |
| [x] | (done 2026-11-01, `ADR-0059`: `KanikoStages`, reflink or data-range copies without special files, the stage root's identity checked per `RUN`; follow-ups: release a stage directory once nothing depends on it, `st_ino`/`st_blocks` in the copy's change check, FICLONE on btrfs/xfs untested) Per-stage directories `builds/<id>/stages/<n>/rootfs` in the state directory, by extraction or reflink copy, never hardlinks (`ADR-0017`); the full sandbox where mounting is possible | `rnk-snapshot`, `rnk-store`, `rnk-sandbox` |
| [x] | (done 2026-11-01 as a refusal: `ADR-0060`, Q-400 (a) — a bare `chroot` on a container's `/` lets a root `RUN` escape (experiment 12) and off `/` stages already run in parallel in the full sandbox (`ADR-0059`); nothing below was built; a hardened bare `chroot` (Q-400 (b)) only after a VM measurement) `--parallel-stages` (the final name, `ADR-0038`, Q-019): `Isolation::BareChroot`, `/dev` by `mknod`, child subreaper, the start-up log line; fake `/proc` with the fourteen files per cgroup version (`ADR-0038`, Q-037); `JAVA_TOOL_OPTIONS` under cgroup limits (`ADR-0019`); the Java warnings and the hints after a failed `RUN` | `rnk-sandbox`, `ranikust` |
| [x] | (done 2026-11-01: `--ignore-path`/`--snapshot-mode` take effect with tests; `--ignore-var-run`/`--whitelist-var-run` refused, ADR-0052; `--materialize` refused until Phase 4, ADR-0064; `--single-snapshot` refused for good, ADR-0065; the ignored class is now refused, ADR-0049) kaniko flags refused until Phase 2 (`ADR-0036`): `--ignore-path`, `--ignore-var-run`, `--whitelist-var-run`, `--single-snapshot`, `--materialize`, `--snapshot-mode` taking effect; the ignored class revisited for kaniko mode | `ranikust` |
| [x] | (done 2026-11-01, task 11: stage-directory span test; `.resolve` and `.syncfs` documented as steps ranikust does not have) Metrics: `rnk_snapshot.baseline`, `.walk`, `.resolve`, `.syncfs` and the deletion spans ([design/metrics.md](../design/metrics.md#rnk-snapshot)) | each crate |
| [x] | (done 2026-11-01, task 11: `rk-kaniko` in a default `docker run`, `rk-kaniko-host` as root off `/`, D2 as a T2 test, D1/D3 mapped in testing.md, leak check per image, disk guards) T2: the VM runs kaniko mode as real root on a host workdir and inside a default `docker run` (no extra capability); deletion tests D1–D3 of experiment 09; the `e2e` suite gets a kaniko-mode leg | tests |

## Task order

Three tracks after task 0: set-up (1, 2), the snapshot (3–6) and execution
(7–11). The snapshot track is the long one.

| # | Task | Needs | Blocked by |
|---|---|---|---|
| 0 | Hash and walk benchmark; ADR with the picks (done, `ADR-0039`) | | |
| 1 | Workdir checks, lock, marker, `RLIMIT_NOFILE` (`ranikust`, `rnk-rootfs`) (done) | | |
| 2 | State directory default, `--mode=auto` picking kaniko mode, the refused flags accepted (done, `ADR-0040`) | 1 | |
| 3 | Ignore list from mountinfo and the fixed paths; extraction into a live root (done, `ADR-0041`) | 1 | |
| 4 | Parallel walk, hash, stat cache, diff during the walk (done) | 0, 3 | |
| 5 | Walk diff → layer exporter (done, `ADR-0045`) | 4 | |
| 6 | `KanikoRoot` (`StageRootfs`, `Snapshotter`) on `/` and per-stage directories; safe deletion between stages (done on the shared root: `ADR-0046`, `ADR-0047`; follow-ups: refuse a mount change between the workdir and its private ancestor, an error instead of a silent skip when the workdir's owner and mode cannot be read, a refusal message saying how to repair a workdir left by a crash) | 3, 4, 5 | |
| 7 | (done 2026-11-01: `ADR-0057`, `ADR-0058`) `RUN` with `Isolation::Direct` on the shared root; placed files and mtime restore. Also, from `ADR-0053`: drop `dir://` (`config.rs` `context_dir`, `context_with_sub_path`, `context_sub_path_ignored`; `cli.rs` help; the `main.rs` warning; `rnk_context::parse_spec` and its tests; `config/tests.rs` `context_sub_path_applies_to_dir_contexts`; `tests/vm/guest/e2e.sh`'s `--context=dir:///workspace`), refuse `dir://` naming the plain path, and refuse (exit 2) `--context-sub-path` with a local context, telling the user to pass the sub-directory as `--context` (`ADR-0054`; replaces the warning in `main.rs` and `context_sub_path_ignored`, and the 'ignored for a plain path' test assertions) | 6 | |
| 8 | Sequential flow: squashing, kept stage outputs, shared-`/` exclusivity in `rnk-sched`, lazy snapshots | 6, 7 | |
| 9 | (done 2026-11-01: `ADR-0059`) Per-stage directories with the full sandbox | 6, 8 | |
| 10 | (done 2026-11-01: refused, `ADR-0060`) `--parallel-stages`: bare `chroot`, `/dev`, subreaper, fake `/proc`, Java | 9 | |
| 11 | (done 2026-11-01 except the full acceptance run; shortened e2e green; Q-410 open) CLI end to end in the VM, the e2e kaniko-mode leg, acceptance runs | all | |

Run in parallel from 2026-10-30, each on its own branch and worktree,
because these parts need only what is already built: task 2; task 3; task
4 against the existing `rnk_rootfs::IgnoreList`, so it does not wait for
task 3's list; the sandbox half of task 7 (`Isolation::Direct`, the placed
files' journal and mtime restore), testable on any root without
`KanikoRoot`; and the logic half of task 8 (squashing, the lazy-snapshot
rule from the stage graph, the shared-`/` exclusivity in `rnk-sched`).
Task 5 waits for task 4's diff type (`rnk_snapshot::WalkDiff`); task 6 joins 3, 4 and 5, and wires
the halves of 7 and 8 in.

From 2026-10-31 (the maintainer's order): first `ADR-0050` (the build
root and the state directory) on its own branch; then, in parallel,
`ADR-0051` (state out of `RUN`'s reach, with the subreaper of task 7) and
the integration half of task 8 (`COPY --from`, `ADR-0052` D2); then the
integration half of task 7 (`Direct`: heredoc `#!`, `RUN --mount`,
`--network=none`) with D1, D4, task 6's follow-ups and the tests from the
issue mining; then tasks 9–11. After Phase 2: the `refactor` pass, then
the security scan and hardening.

Every task instruments its crate as its card's `## Metrics` and
[design/metrics.md](../design/metrics.md#spans-per-crate) list, tested
with `rnk_metrics::Capture`; a task is not done without it. Tasks 1, 3, 6,
7, 9 and 10 touch `rnk-rootfs`, `rnk-snapshot`, `rnk-sandbox` or
`rnk-store`: plan mode first and the `host-safety` subagent on the diff.

## Acceptance criteria

- [x] (met 2026-11-03, final ultra run at `3992fde`: default `docker run` picked kaniko mode on `/` and per-stage directories as root in the VM built W1, W2 cold and W3, 5/5 rounds each) Builds W1, W2 cold and W3 in sequential kaniko mode inside a default
      `docker run` (root, no `CAP_SYS_ADMIN`) in the VM, and as root on a
      host, in a root of the state directory (ADR-0050).
- [x] (met 2026-11-03, final ultra run: `other=0`, `config=0` against kaniko on W1–W3, noise only the known files; one W2 file, `/app/assets/MANIFEST.sha256`, differs for per-stage directories by the workload's own race of `find` against its redirect, as on 2026-10-30) Flattened content and config compared with kaniko's on W1–W3: only
      `ADR-0012` differences and the known non-deterministic files.
- [x] (met 2026-11-03: final ultra run at `3992fde` gave walk and hash on W1 2.82 s (on `/`) and 2.19 s (per-stage) against 3.18 s, and every build faster than kaniko but kaniko mode on `/` on W2, +3.9 %; fixed by ADR-0081 (a base layer verified once per build on `/`, decompressed beside its extraction): W2 on `/` 14.49 s against kaniko's 15.15 s, W1 19.64 against 20.49, W3 16.04 against 26.26, narrow VM run of 3 rounds) Time per build and per snapshot phase compared with kaniko's metrics
      report: the walk and hash of every step take less than kaniko's
      `snapshot.walk` + `snapshot.hash` (3.18 s on W1, `exp-07`), and no
      build is slower than kaniko's.
- [x] (met 2026-11-03: ultra `leaks: none` on all 13 kaniko-mode and overlay images; the stub and mount checks `smoke_check_no_stub_leak` and the quick `hygiene`/`hygiene-mounts` cases green in the VM at `3992fde`) No mount point (`/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname`,
      volumes), no file of the state directory and no placed file reaches a
      layer; `/etc` is not in a layer only because of a placed file.
- [x] (reworded 2026-11-03 by the maintainer: the workdir is gone since
      ADR-0050; met 2026-11-03: T0 `statedir` and `rnk-store` refusals, T2c `a_second_build_on_slash_is_refused_by_the_lock` and the VM quick check `slash_lock_refuses_a_second_build`, T2c `mounts_inside_the_root_survive`, `prune_refuses_a_mount_below_the_root`, `a_mount_where_clean_up_deletes_is_not_emptied`, T0 `a_second_build_empties_slash_but_never_the_containers_markers_or_mounts`) Host safety: `/`, every system directory and their
      ancestors refused as the state directory by `(dev, ino)`; a non-empty
      state directory without `.ranikust-state` refused; a second
      kaniko-mode build on `/` in one container refused by the flock;
      deletion keeps a same-filesystem bind mount (D1–D3 of experiment 09).
      T0 and T2.
- [x] (met 2026-11-03: ultra W3 per-stage `--reproducible` gives one digest at `--jobs` 1, 2 and default; the quick `reproducible` case, builds a second apart, green at `3992fde`) `--reproducible`: two builds at different times give one digest; in
      per-stage mode the digest does not depend on `--jobs`.
- [x] (reworded 2026-11-01 by the maintainer, "2. ok"; met: `par-stages` bench in the VM, per-stage kaniko mode −43…−45 % and overlay −38 % against the same stages chained, at a 4-CPU cap and uncapped; peak memory recorded per build in e2e) With the same CPU budget, per-stage directories (sandboxed) build independent stages at least 40 % faster than the same stages chained, on the `par-stages` case at a 4-CPU cap; W3 at 4 CPUs no worse than experiment 12's −21 %; experiment 12's −41…−44 % on W3 is a 32-CPU figure, measured only by the ultra benchmark; peak memory recorded. (`--parallel-stages` is refused, `ADR-0060`; the fake `/proc` and bare `chroot` parts no longer apply.)
- [x] (done 2026-11-01: flags tested for their meaning, kaniko-map and the book updated; `--materialize` refused, ADR-0064; `--single-snapshot` refused pending Q-420; `--mode=auto` routes an unusable overlay to per-stage kaniko mode) The kaniko-mode flags of the scope are tested for their meaning, and
      [kaniko-map.md](kaniko-map.md#flags) and the book are updated.
- [x] (CI run 41308092662, 2026-11-01) aarch64 T0 green in CI, as in Phase 1.

## Carried over from Phase 1

Not blocking Phase 2; each lands when its owner is next touched:

- config goldens against kaniko and BuildKit (`rnk-builder`);
- the W3 digest compared by content without the workload's `du` output
  ([Q-187](../open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output));
- W1 and W3 metrics reports compared with kaniko's by the bench harness;
- T2 on cgroup v1 and kernels before 5.11, and a CI job with KVM.

## Carried over to later phases

Decided, not built; each lands with its phase or when its owner is next
touched:

- [ADR-0082](../adr/0082-full-sandbox-binds-the-binary-read-only-over-itself.md):
  bind ranikust's binary read-only over itself in the full sandbox's mount
  namespace (Q-422 (b)), with a T2 test; `Direct` on `/` keeps its barriers
  (Q-422 (d)). Reword the "open question Q-422" comments in
  `rnk-sandbox` `spawner.rs` and `tests/smoke/cases/self-exe` then.
- [ADR-0083](../adr/0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md):
  a cgroup of its own and `cgroup.kill` for a `RUN`'s command where a
  delegated cgroup v2 tree exists (Q-452 (a)); a user namespace that
  leaves the init's uid unmapped with Phase 3's `--userns` (Q-452 (b)).
- [ADR-0084](../adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md):
  secret files in kaniko mode on `/`, their place on the ignore list and
  the warning (Q-445), once
  [Q-453](../open-questions.md) says where the file goes.
- Open questions left for their phases: Q-438 (context reads with the
  owner's credentials, Phase 5), Q-441 (a cache for kaniko mode on `/`,
  with Phase 4's layer cache), Q-444 (`zeroize`, Phase 6), Q-447
  (`--network=none` and ranikust's own requests), Q-448 (a pull's overall
  deadline), Q-449 (push-side bodies).
- Backlog raised for the hardening pass and not built: cosign signing of
  images and cache entries; `chain()` on `rnk_sched` job types to drop
  `ranikust`'s `unit_chain`; the `unescape` shared by `rnk-sandbox` and
  `rnk-snapshot`; the Phase 1 `_` arms in `rnk-sandbox` `plan.rs` and
  `journal.rs`.
- The Phase 1 items listed above under Carried over from Phase 1 that are
  still open.

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
