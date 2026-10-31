# Stage scheduler

How ranikust runs the stages of a Dockerfile: as a dependency graph, in
parallel where the stage roots allow it, with deterministic output.

Decisions: [ADR-0013](../adr/0013-jobs-default-available-parallelism.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md).

## Invariants

- **Output never depends on execution order**: layers, image config and
  manifest depend only on content. `--jobs 1` and the default produce the
  same digests; a test enforces it under `--reproducible`.
- Instructions inside one stage run in order. Parallelism exists only between
  stages and in I/O (pulls, compression, pushes).
- A stage whose root is the shared `/` runs alone: no other stage touches a
  root while it runs.
- `rnk-sched` does not know how a stage is built; it calls `rnk-builder`.

## The graph

Edges between stages:

- `FROM <stage>`;
- `COPY --from=<stage>` (and `ADD --from`, if supported);
- `RUN --mount=type=bind|cache,from=<stage>`;
- `ONBUILD` triggers of a stage used as a base.

Stages not on a path to `--target` (or the last stage) are dropped, like
kaniko's `MakeKanikoStages`. In overlay mode `FROM <stage>` just reuses the
parent's lower stack, so parent stages are never squashed into children.

`rnk_dockerfile::stage_graph` builds this graph with BuildKit's rules
(`dockerfile2llb`): `ADD` has no `--from`; a reference names a stage
defined earlier in the file (a later one is an error, as in BuildKit,
which builds stages in file order); anything that is not a stage name is
an image the scheduler pulls up front. The graph takes a list of targets,
the first being the result; `ranikust` passes at most one
([ADR-0049](../adr/0049-security-and-performance-over-kaniko-compatibility.md)).

## Execution

- Each stage is an async task that starts when all stages it depends on have
  finished.
- Work that touches no root (resolving and pulling base images, cache
  lookups) starts for **all** stages at once, at the beginning of the build.
- `COPY --from=<stage>` reads the source stage's final root directly
  (read-only mount in overlay mode); there is no intermediate copy and no
  deletion between stages.
- The capability of each `StageRootfs` (shared `/` or private root) decides
  whether the stage may run concurrently with others.
- How a stage is built is behind a trait, `StageRunner`: `prepare` is the
  work that touches no root, `build` the stage itself. The scheduler drives
  every stage's future from the task that awaits it, without
  `tokio::spawn`; the builder moves blocking work to its own threads. It
  waits for every started call to return before it returns, since
  clean-up cannot rely on `Drop`.
- The stages' outputs come back in file order, whatever order they
  finished in; a stage sees its dependencies' outputs in
  `StageNode::dependencies()` order.

## Limits

- **`--jobs N`** bounds the number of `RUN` commands running at once across
  all stages. Default: `std::thread::available_parallelism()`, which honours
  the cgroup CPU quota and the affinity mask. Network transfers and
  compression have their own limits.
- **Memory-aware limit**: memory, not CPU, is the binding constraint for
  parallel stages. Before starting another stage, the scheduler checks
  memory headroom: the cgroup's working set (`memory.current` less
  `inactive_file`) against `memory.max` when a limit exists, otherwise
  `MemAvailable`. In kaniko mode the estimate includes the
  page cache each running stage will re-read to hash and tar its writes
  [inferred: the exact algorithm is not designed yet]. The gate sits in
  front of each stage, not each `RUN`: a stage starts while free memory is
  at least an eighth of the limit and at least 512 MiB, and always when no
  stage runs, so the gate can slow a build but not stop it. It admits at
  most one stage per fresh reading, taken 500 ms or more after the last
  admission, so stages that become ready together do not all start on the
  same free memory. These numbers
  are a placeholder until measured
  ([Q-030](../open-questions.md#q-030-what-are-the-memory-limit-algorithm-and-thresholds)).
- Sequential kaniko mode ignores `--jobs` for stage execution; pulls and
  compression still run in parallel.

## Lazy export

A layer is encoded (tar, compress, digest) only if it will be pushed, cached
(`--cache`), written as a tar or OCI layout, or belongs to the `--target`
stage. Layers of stages used only through `COPY --from` or `FROM <stage>` are
never encoded. In kaniko mode such stages are not even snapshotted
([kaniko-mode](kaniko-mode.md#lazy-snapshots)).

When a layer is needed it is exported as soon as it is complete, overlapping
with the next instruction, while its pages are still cached. A cap on
compression threads keeps export from starving running commands.

## Failure and cancellation

- The first failing stage cancels all others: kill their `RUN` process trees
  (by PID namespace where one exists), unmount, clean up. The exit code is
  that of the first failure, as in BuildKit.
- Release builds abort on panic, so cleanup never relies on unwinding or
  `Drop`: mounts live in the private mount namespace, `RUN` children have
  `PR_SET_PDEATHSIG`, and stubs are journaled before creation
  ([sandbox](sandbox.md)).

## Logging

Every log line is prefixed `[<stage-name> <k>/<n>]`. An option groups output
per stage for CI logs that are read top to bottom. Log interleaving may vary
between runs; build output may not.

## Later

Two stages with the same lower stack and the same cache key for an
instruction run it once (BuildKit deduplicates such vertices).

## Evidence

| W3, three independent stages | 32 CPU | 4 CPU |
|---|---|---|
| Overlay, lazy, `--jobs 1` | 8.44 s | 11.35 s |
| Overlay, lazy, `--jobs` default | 6.02 s | 10.73 s |
| Overlay, export every layer, `--jobs` default | 12.69 s | 12.99 s |
| kaniko mode, sequential | 14.04 s | 17.14 s |
| kaniko mode, parallel bare `chroot`, lazy | 5.38 s | 10.02 s |

- [Experiment 08](../research/experiments/08-parallel-stages-lazy-export.md):
  no lazy configuration was slower with the default `--jobs` than with
  `--jobs 1`; lazy export saved 1.3–1.7 s sequentially and up to 6.6 s in
  parallel under memory pressure; peak memory rose from 4.1–4.5 GB to
  6.6–7.3 GB, and exports re-read 183–538 MB per layer from disk near the
  8 GB limit.
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md) perf
  §5.5: parallel walk on W3 needs 8.2–8.3 GB including page cache, against
  5.4–6.0 GB sequentially; with an 8 GB cap it hit the limit in 46 of 60 runs.
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §4.3:
  export reads layers back from disk when their pages were reclaimed; export
  right after the layer keeps them warm.
- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §4.

## Open questions

- [Q-030](../open-questions.md#q-030-what-are-the-memory-limit-algorithm-and-thresholds) What are the memory-limit algorithm and thresholds?
- [Q-033](../open-questions.md#q-033-which-performance-conditions-are-still-unmeasured) Which performance conditions are still unmeasured?
