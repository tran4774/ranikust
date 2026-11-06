# Stage scheduler

**TL;DR:** `rnk-sched` runs a Dockerfile's stages as a dependency graph, in
parallel where the stage roots allow it, and the output never depends on the
order they ran in.

Decisions: [ADR-0013](../adr/0013-jobs-default-available-parallelism.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md).

## Invariants

- **Output never depends on execution order.** Layers, image config and
  manifest depend only on content. `--jobs 1` and the default give the same
  digests; a test enforces it under `--reproducible`.
- Instructions inside one stage run in order. Parallelism exists only
  between stages and in I/O (pulls, compression, pushes).
- A stage whose root is the shared `/` runs alone.
- `rnk-sched` does not know how a stage is built; it calls `rnk-builder`.

## The graph

Edges between stages:

- `FROM <stage>`;
- `COPY --from=<stage>`;
- `RUN --mount=type=bind|cache,from=<stage>`;
- `ONBUILD` triggers of a stage used as a base.

`rnk_dockerfile::stage_graph` builds it with BuildKit's rules
(`dockerfile2llb`):

- `ADD` has no `--from`.
- A reference names a stage defined earlier in the file; a later one is an
  error.
- Anything that is not a stage name is an image the scheduler pulls up front.
- It takes a list of targets, the first being the result; `ranikust` passes at
  most one ([ADR-0049](../adr/0049-security-and-performance-over-kaniko-compatibility.md)).

Stages not on a path to `--target` (or the last stage) are dropped, like
kaniko's `MakeKanikoStages`. In overlay mode `FROM <stage>` reuses the
parent's lower stack, so a parent is never squashed into its children.

## Execution

- Each stage is an async task that starts when the stages it depends on have
  finished.
- Work that touches no root (resolving and pulling base images, cache
  lookups) starts for **all** stages at the beginning of the build.
- `COPY --from=<stage>` reads the source stage's final root directly
  (read-only mount in overlay mode): no intermediate copy, no deletion
  between stages.
- The capability of each `StageRootfs` (shared `/` or private root) decides
  whether a stage may run beside others.
- `StageRunner` is the trait behind how a stage is built: `prepare` is the
  work that touches no root, `build` the stage itself. The scheduler polls
  every stage's future from the task that awaits it, without `tokio::spawn`;
  the builder moves blocking work to its own threads. The scheduler returns
  only after every started call has, since clean-up cannot rely on `Drop`.
- Outputs come back in file order, whatever order the stages finished in; a
  stage sees its dependencies' outputs in `StageNode::dependencies()` order.

## Limits

- **`--jobs N`** bounds the `RUN` commands running at once across all
  stages. Default: `std::thread::available_parallelism()`, which honours the
  cgroup CPU quota and the affinity mask. Network transfers and compression
  have limits of their own.
- **Memory gate.** Memory, not CPU, binds parallel stages. The gate sits in
  front of each stage, not each `RUN`:
  - It reads free memory: the working set (`memory.current` less
    `inactive_file`) of ranikust's cgroup, or of its parent when that
    cgroup is the leaf `ranikust-<pid>` that `rnk-sandbox` nests it in
    (only `cpu`, ADR-0110), and of no cgroup further up, against
    `memory.max` when a limit exists, otherwise `MemAvailable`.
  - A stage starts while free memory is at least an eighth of the limit and
    at least 512 MiB, and always when no stage runs, so the gate slows a
    build but never stops it.
  - It admits at most one stage per fresh reading, taken 500 ms or more
    after the last admission, so stages that become ready together do not
    all start on the same free memory.
  - In kaniko mode the estimate should include the page cache each running
    stage re-reads to hash and tar its writes [inferred: not designed yet].
  - The numbers are a placeholder until measured
    ([Q-030](https://github.com/tran4774/ranikust/issues/9)).
- Sequential kaniko mode ignores `--jobs` for stage execution; pulls and
  compression still run in parallel.

## Lazy export

A layer is encoded (tar, compress, digest) only if it will be pushed, cached
(`--cache`), written as a tar or OCI layout, or belongs to the `--target`
stage. Layers of stages used only through `COPY --from` or `FROM <stage>` are
never encoded; in kaniko mode such stages are not even snapshotted
([kaniko-mode](kaniko-mode.md#lazy-snapshots)).

A needed layer is exported as soon as it is complete, overlapping the next
instruction while its pages are still cached. A cap on compression threads
keeps export from starving running commands.

## Failure and cancellation

- The first failing stage cancels all others: their `RUN` process trees are
  killed (by PID namespace where one exists), mounts released, state cleaned
  up. The exit code is that of the first failure, as in BuildKit.
- Release builds abort on panic, so clean-up never relies on unwinding or
  `Drop`: mounts live in the private mount namespace, `RUN` children have
  `PR_SET_PDEATHSIG`, and stubs are journaled before creation
  ([sandbox](sandbox.md)).

## Logging

Every log line is prefixed `[<stage-name> <k>/<n>]`. Interleaving may vary
between runs; build output may not. Grouping output per stage is not built.

## Not built

Two stages with the same lower stack and the same cache key for an
instruction should run it once (BuildKit deduplicates such vertices).

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
  `--jobs 1`; lazy export saved 1.3-1.7 s sequentially and up to 6.6 s in
  parallel under memory pressure, at the cost of 6.6-7.3 GB peak memory
  against 4.1-4.5 GB.
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md) perf
  §5.5: a parallel walk on W3 needs 8.2-8.3 GB including page cache, against
  5.4-6.0 GB sequentially; under an 8 GB cap it hit the limit in 46 of 60
  runs.
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §4.3: export
  right after the layer keeps its pages warm; later it reads them back from
  disk.
- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §4.

## Open questions

- [Q-030](https://github.com/tran4774/ranikust/issues/9) What are the memory-limit algorithm and thresholds?
- [Q-421](https://github.com/tran4774/ranikust/issues/29) Which performance conditions does Phase 6 measure?
