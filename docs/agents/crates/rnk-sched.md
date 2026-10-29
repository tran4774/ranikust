# rnk-sched

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** `rnk-builder`, `rnk-dockerfile`, `rnk-registry`, `rnk-cache` · **Used by:** `ranikust` · **Path:** `crates/rnk-sched/`

**TL;DR:** Runs the stage DAG: each stage is an async task that starts when
its dependencies finish, `--jobs` bounds concurrent `RUN`s, memory bounds
concurrent stages, a stage on the shared `/` runs alone, the first failure
cancels the rest. Output never depends on execution order: `--jobs 1` and the
default give the same digests.

## Owns

- One async task per stage, started when every stage it depends on is done.
- Root-independent work for all stages up front: base pulls, cache lookups.
- Limits: `--jobs` on `RUN`s, memory on stages, exclusivity for a shared `/`.
- Cancellation on the first failure, the exit code, per-stage log tags.

## Must not

- Know how a stage is built (`rnk-builder`) or run `RUN` (`rnk-sandbox`).
- Let output depend on execution order, thread count or timing.
- Build the graph (`rnk-dockerfile`) or parse flags (`ranikust`).
- Rely on `Drop` or unwinding for cleanup: release builds abort on panic.

## API (planned)

```rust
pub struct Scheduler { jobs: NonZeroUsize, memory: MemoryBudget }  // [design]
impl Scheduler {
    pub fn new(jobs: Option<NonZeroUsize>) -> Self;  // [decided] default available_parallelism(), ADR-0013
    pub async fn run<F, Fut>(&self, graph: &StageGraph, build_stage: F) -> Result<BuildSummary>
    where F: Fn(StageJob) -> Fut, Fut: Future<Output = Result<StageDone>>;  // [design] a closure: T0 uses a fake builder
}
pub struct StageJob { stage: StageId, deps: Vec<StageDone>, run_slots: RunSlots,
                      cancel: CancellationToken, demand: LayerDemand }        // [design]
pub struct RunSlots;      // [design] one --jobs semaphore for all stages; a permit per RUN
pub struct MemoryBudget;  // [open: Q-030] memory.current vs memory.max, else MemAvailable; algorithm not designed
```

`LayerDemand` is `rnk-builder`'s; which crate derives it from the graph and
the outputs is [open: Q-029]. Log lines carry `[<stage-name> <k>/<n>]` [design].

## Invariants

- Layers, config and manifest depend only on content: `--jobs 1` and the
  default give identical digests, enforced by a test (ADR-0012).
- Instructions in a stage run in order; parallelism is only between stages
  and in I/O (pulls, compression, pushes).
- `--jobs` counts `RUN`s across all stages; the default honours the cgroup
  CPU quota and the affinity mask and is at least 1.
- A stage whose root is `Shared` (`/`) runs alone; sequential kaniko mode
  ignores `--jobs` for stages, while pulls and compression stay parallel.
- Edges: `FROM <stage>`, `COPY --from`, `RUN --mount=...,from=<stage>`,
  `ONBUILD` of a base stage. Stages off the path to `--target` (or the last
  stage) are dropped; overlay mode never squashes stages.
- The first failure cancels the others (their `RUN` trees die with their PID
  namespaces); the exit code is the first failure's, as in BuildKit.
- No global state: `BuildPaths` and configuration travel as values. Log
  interleaving may vary between runs; build output may not.

## Refs

- **Design:** [`design/scheduler.md#the-graph`](../../design/scheduler.md#the-graph), [`#execution`](../../design/scheduler.md#execution), [`#limits`](../../design/scheduler.md#limits), [`#lazy-export`](../../design/scheduler.md#lazy-export), [`#failure-and-cancellation`](../../design/scheduler.md#failure-and-cancellation), [`#evidence`](../../design/scheduler.md#evidence); [`design/modes-and-detection.md#outcomes`](../../design/modes-and-detection.md#outcomes) (which outcomes run stages in parallel)
- **Decisions:** [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md) (`--jobs` default, memory limit), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (sequential kaniko mode), [ADR-0007](../../adr/0007-run-anywhere-shared-store.md) (cgroups are only read), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (same digests within a mode)
- **Evidence:** [exp-08](../../research/experiments/08-parallel-stages-lazy-export.md) (W3 6.02 s vs 8.44 s; peak memory), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) (`--jobs` and memory), [exp-05](../../research/experiments/05-rust-ecosystem.md) Q6 (`available_parallelism`), [exp-16](../../research/experiments/16-cgroup-v1-cpuset.md) (W3 under cgroup v1)
- **kaniko:** `kaniko@44aa7ddc0 pkg/dockerfile/dockerfile.go:297`
  `MakeKanikoStages` drops unused stages, `:393-406` squashes a parent into
  its only child; `pkg/executor/build.go:1410` `CalculateDependencies`;
  `:1978` the sequential stage loop; `:2152-2182` saves `COPY --from` files,
  then empties the filesystem, which overlay mode avoids.
- **External:** <https://doc.rust-lang.org/std/thread/fn.available_parallelism.html>, <https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html>

## Tests

- **T0** (fake builder closure): random DAGs and completion orders give the
  same outputs (property test); permits never exceed `--jobs`; a shared root
  never overlaps another stage; a failure cancels siblings and returns the
  first exit code; pruning to `--target`; up-front work starts for all stages.
- **T1/T2:** W3 with `--jobs` 1, 2 and default gives one digest; a cancelled
  build leaves no mount, process or stub (T2, VM only). Timing comes from the
  `nextjs-kaniko` harness, not from tests.

## Metrics

- `rnk_sched.run` (`stages`, `jobs`, `max_running`); the stage record
  `rnk_sched.stage` (`stage`, `name`, `base`, `max_memory_bytes`);
  `wait_deps`, `wait_memory`, `wait_jobs` ([rows](../../design/metrics.md#rnk-sched)).
- Each stage task is `.instrument`ed with its stage span, so all its work
  lands in its record (`exp-12` per-stage times).

## Pitfalls

- Memory, not CPU, bounds parallelism: W3 peak 4.1–4.5 → 6.6–7.3 GB
  (exp-08 finding 6); parallel kaniko mode needed 8.2–8.3 GB with page cache
  and hit an 8 GB limit in 46 of 60 runs (exp-12 finding 12).
- Parallel plus exporting every layer was 3 s slower at 32 CPUs, because
  exports re-read from disk under memory pressure (exp-08 finding 3).
- "Parallel is slower" came from OrbStack and did not reproduce on native
  Linux; do not cite it (exp-08 findings 2, 4).
- W3 has three stages, so it cannot tell "cores" from "stages" (ADR-0013).
- `available_parallelism()` ignores `--cpu-shares` and rounds a fractional
  quota down (exp-05 Q6).
- A stage skipped by lazy snapshots that later needs a layer must still
  detect deletions (kaniko-mode design, open questions).

## Phase 1

- Build: graph execution on tokio, up-front base pulls, `RunSlots` with the
  default `--jobs`, cancellation and exit code, log tags, the digest test;
  measure W3 with `--jobs` 1, 2 and default.
- Memory-aware limit: decided (ADR-0013); add the hook and measure now, the
  thresholds stay [open: Q-030] until measured.
- Deferred: shared-`/` exclusivity and `--parallel-stages` (Phase 2); cache
  lookahead and skipping fully cached stages (Phase 4); `ONBUILD` and
  `RUN --mount=type=bind,from=` edges (Phase 5); deduplicating identical
  instructions across stages (later).
