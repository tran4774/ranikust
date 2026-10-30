# rnk-sched

> **Status:** in progress · **Phase:** 1 ·
> **Depends on:** `rnk-dockerfile` ([open: Q-028], interim), `tokio`, `tokio-util`, `futures-util`; later `rnk-builder`, `rnk-registry`, `rnk-cache` · **Used by:** `ranikust` · **Path:** `crates/rnk-sched/`

**TL;DR:** Runs the stage DAG: each stage is an async task that starts when
its dependencies finish, `--jobs` bounds concurrent `RUN`s, memory bounds
concurrent stages, a stage on the shared `/` runs alone (Phase 2), the first
failure cancels the rest. Output never depends on execution order: `--jobs 1`
and the default give the same digests.

## Owns

- One async task per stage, started when every stage it depends on is done.
- Root-independent work for all stages up front: base pulls, cache lookups.
- Limits: `--jobs` on `RUN`s, memory on stages, exclusivity for a shared `/`
  (Phase 2).
- Cancellation on the first failure, the exit code, per-stage log tags.

## Must not

- Know how a stage is built (`rnk-builder`) or run `RUN` (`rnk-sandbox`).
- Let output depend on execution order, thread count or timing.
- Build the graph (`rnk-dockerfile`) or parse flags (`ranikust`).
- Rely on `Drop` or unwinding for cleanup: release builds abort on panic.

## API

```rust
pub struct Scheduler;  // jobs + a Box<dyn MemoryGate>
impl Scheduler {
    pub fn new(jobs: Option<NonZeroUsize>) -> Self;  // default available_parallelism() (ADR-0013), SystemMemory gate
    pub fn with_memory_gate(self, gate: impl MemoryGate + 'static) -> Self;
    pub fn jobs(&self) -> NonZeroUsize;
    pub async fn run<R: StageRunner>(&self, graph: &StageGraph, runner: &R, cancel: &CancellationToken)
        -> Result<BuildSummary<R::Output>, Error<R::Error>>;  // needs a tokio runtime with the timer; = run_units(own_root_units)
    pub async fn run_units<R: StageRunner>(&self, graph, units: &[Unit], runner, cancel)
        -> Result<BuildSummary<R::Output>, Error<R::Error>>;  // explicit units, dependencies earlier in the list
}
pub enum Placement { Own, Shared }   // Shared: the `/` of sequential kaniko mode, one unit at a time, in unit order
pub struct Unit { pub stage: usize, pub folded: Vec<usize>, pub placement: Placement }  // Unit::own(stage), chain()
pub fn own_root_units(g: &StageGraph) -> Vec<Unit>;               // graph.needed, Own
pub fn shared_root_units(plan: &[rnk_dockerfile::SquashedStage]) -> Vec<Unit>; // kaniko's squashed plan, Shared
pub trait StageRunner: Sync {          // rnk-builder's side; T0 uses fakes
    type Prepared: Send; type Output: Send + Sync; type Error: std::error::Error + Send + Sync + 'static;
    fn prepare(&self, job: PrepareJob<'_>) -> impl Future<Output = Result<Self::Prepared, Self::Error>> + Send;
    fn build(&self, job: StageJob<'_, Self>) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
}
pub struct PrepareJob<'a> { pub stage: usize, pub node: &'a StageNode, pub folded: &'a [usize],
                            pub placement: Placement, pub graph: &'a StageGraph,
                            pub tag: LogTag, pub cancel: CancellationToken }
pub struct StageJob<'a, R> { pub stage, pub node, pub folded, pub placement, pub graph, pub deps: Vec<(usize, Arc<R::Output>)>,
                             pub prepared: R::Prepared, pub tag: LogTag, pub cancel: CancellationToken, .. }
impl StageJob<'a, R> { pub async fn run_slot(&self) -> Option<RunPermit<'a>>; pub fn slots(&self) -> &'a RunSlots; }
pub struct RunSlots;   // one --jobs semaphore for all stages; a permit per RUN
pub trait MemoryGate: Send + Sync { fn check(&self, running: usize) -> MemoryCheck; }
pub struct MemoryCheck { pub admit: bool, pub current_bytes: Option<u64> }
pub struct SystemMemory;  // cgroup v2 memory.max - memory.current, else MemAvailable
pub struct Unlimited;
pub struct LogTag { pub name: String, pub k: usize, pub n: usize }  // Display: [name k/n]
pub struct BuildSummary<O> { pub stages: Vec<(usize, Arc<O>)>, pub target: usize }  // file order
pub enum Error<E> { Stage { stage, name, source: E }, Prepare { .. }, Cancelled, Graph { stage, reason } }
pub use tokio_util::sync::CancellationToken;
```

- `run` builds `graph.needed` only (the targets' stages): `prepare` starts
  for every one of them before any `build`; a stage's `build` starts when
  its `prepare` and its dependencies are done and the memory gate admits
  it. `deps` are in `StageNode::dependencies()` order, base first.
- `run_units` runs explicit units: a unit's `deps` are the dependencies
  of every stage of its chain (folded stages first) outside the chain. A
  `Shared` unit waits, besides its dependencies, for the `Shared` unit
  before it in the list: shared stages never overlap and run in list
  order whatever finishes first; `Own` units still run beside them. The
  caller warns once that `--jobs` changes nothing on the shared root.
- The scheduler drives every stage's future from the one task that awaits
  `run`, so a runner moves blocking work (a `RUN`, extraction,
  compression) to its own threads. Nothing is `tokio::spawn`ed, so the
  runner and graph are borrowed, not `'static`.
- `rnk-builder` must not depend on `rnk-sched` (lower crates never depend
  on higher ones): the `StageRunner` impl for the builder lives here, or in
  `ranikust`, once `rnk-builder` exists.
- `LayerDemand` is `rnk-builder`'s; which crate derives it from the graph
  and the outputs is [open: Q-029]. `StageJob` carries `graph` and `stage`,
  enough for either answer.
- Memory gate: another stage starts while free memory (cgroup `memory.max`
  less the working set, `memory.current` minus `inactive_file`; else
  `MemAvailable`) is at least an eighth of the limit, and at least
  512 MiB; at most one stage per fresh reading (500 ms after the last
  admission); a stage always starts when none runs; a stage held back asks
  again when one finishes, at the latest every 250 ms. The algorithm and
  thresholds are [open: Q-030]; the gate is a trait, so the placeholder is
  one function and two constants.
- `--jobs` relies on the runner: `build` must call `run_slot` before each
  `RUN` (ADR-0013). Later, the `RUN` entry point (`rnk-sandbox` through
  `rnk-builder`) should take a `&RunPermit`, so a `RUN` without a permit
  does not compile.
- Clean-up belongs to the runner: on failure `run` drops the outputs and
  unbuilt `Prepared` values, so the runner journals and removes its roots
  and pulls itself. `run` is not cancel safe: cancel the token and keep
  awaiting.
- Log lines carry `[<stage-name> <k>/<n>]`, `k` the position among the
  stages the build runs; grouping per stage would be an option of its
  own, apart from `--log-format` (Q-031, ADR-0036), not built yet.

## Invariants

- Layers, config and manifest depend only on content: `--jobs 1` and the
  default give identical digests, enforced by a test (ADR-0012).
- Instructions in a stage run in order; parallelism is only between stages
  and in I/O (pulls, compression, pushes).
- `--jobs` counts `RUN`s across all stages; the default honours the cgroup
  CPU quota and the affinity mask and is at least 1.
- A unit whose root is `Shared` (`/`) never overlaps another shared unit,
  and shared units run in the order given; pulls (`prepare`) and
  compression stay parallel (`run_units`).
- Edges: `FROM <stage>`, `COPY --from`, `RUN --mount=...,from=<stage>`,
  `ONBUILD` of a base stage. Stages off the path to `--target` (or the last
  stage) are dropped; overlay mode never squashes stages, sequential
  kaniko mode runs `rnk_dockerfile::squash_for_shared_root`'s units.
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
  never overlaps another stage and keeps unit order on a multi-thread
  runtime, a squashed chain's folded `COPY --from` becomes its dependency,
  units out of order refused (`tests/units.rs`); a failure cancels siblings and returns the
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
- Done (T0, fakes): the DAG, up-front `prepare`, `--jobs`, the memory-gate
  hook with a placeholder policy, cancellation on the first failure or the
  caller's token, log tags, outputs in file order (property test over
  random DAGs, completion orders and `--jobs`), the spans. Left: the
  `StageRunner` impl for `rnk-builder` (task 11), the W3 digest test with
  `--jobs` 1, 2 and default (task 13), measuring the gate (Q-030), and
  Phase 2: `Placement::Shared` units run one at a time in unit order
  (`run_units`, built).
  The
  first failure is the first by completion time, as in BuildKit; two
  failures at once may be reported either way.
- Memory-aware limit: decided (ADR-0013); add the hook and measure now, the
  thresholds stay [open: Q-030] until measured.
- Deferred: `--parallel-stages` (Phase 2 task 10); cache
  lookahead and skipping fully cached stages (Phase 4); `ONBUILD` and
  `RUN --mount=type=bind,from=` edges (Phase 5); deduplicating identical
  instructions across stages (later).
