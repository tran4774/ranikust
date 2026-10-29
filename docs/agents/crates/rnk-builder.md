# rnk-builder

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** `rnk-snapshot`, `rnk-sandbox`, `rnk-cache`, `rnk-layer`, `rnk-registry`, `rnk-context`, `rnk-ignore`, `rnk-dockerfile`, `rnk-store`, `rnk-rootfs`, `rnk-oci` · **Used by:** `rnk-sched` · **Path:** `crates/rnk-builder/`

**TL;DR:** Builds one stage: one executor per instruction, diffs through
`rnk-snapshot`, `RUN` through `rnk-sandbox`, encoding only the layers that are
needed (lazy export), image config updates, then image assembly and output.
Keep it free of global state (paths come as `BuildPaths`) and its output
independent of timing.

## Owns

- One executor per instruction (the `commands` module) and cache checks.
- Lazy layer export: a layer is encoded only when it is pushed, cached or
  part of the target.
- Image config updates; image assembly and output: registry push, Docker tar,
  OCI layout.

## Must not

- Keep global state (no process-wide cwd, `PATH` or environment changes):
  paths arrive as `rnk-store`'s `BuildPaths`.
- Order stages (`rnk-sched`), parse flags (`ranikust`) or the Dockerfile
  (`rnk-dockerfile`); mount or diff (`rnk-snapshot`), fork `RUN`
  (`rnk-sandbox`), compute keys (`rnk-cache`), write tar (`rnk-layer`).
- Touch a path in a root except through `RootPath`.

## API (planned)

```rust
use rnk_store::BuildPaths;  // [design] { root, state }, the only source of paths (ADR-0021)
pub struct StageBuilder<'a> { /* &BuildPaths, &Sandbox, snapshotter, registry, cache */ }  // [design]
impl StageBuilder<'_> {
    pub async fn build(&self, job: StageJob, root: impl StageRootfs) -> Result<StageDone>;  // [design] called by rnk-sched
}
pub trait Executor {                                 // [design] one per instruction, `commands` module
    fn touches_rootfs(&self) -> bool;                // false for ENV, LABEL, USER...: no mount, no layer
    async fn execute(&self, cx: &mut StageCx<'_>) -> Result<()>;  // files via cx.root or cx.sandbox; config via cx.config
    fn cache_inputs(&self, cx: &StageCx<'_>) -> Result<KeyInputs>;  // [open: Q-041] Phase 4
}
pub struct LayerDemand { pub push: bool, pub cache: bool, pub target: bool, pub output: bool }  // [design] encode iff any
pub enum Output { Registry(Reference), DockerTar(PathBuf), OciLayout(PathBuf) }                 // [design]
```

## Invariants

- Instructions run in order inside a stage; metadata-only instructions mount
  nothing and add no layer.
- `COPY`/`ADD` write through the stage root (`rootfs/` in overlay mode), so
  destination symlinks resolve as in kaniko; this crate decides which context
  files they read, with `rnk-ignore`; `--chown` names resolve through
  `rnk-rootfs` in the stage root.
- A layer is encoded only if pushed, cached, written as a tar or OCI layout,
  or in the `--target` stage; then it is exported as soon as it is complete,
  overlapping the next instruction, with a cap on compression threads.
- Same inputs, same bytes: layer order is instruction order whatever order
  exports finish in; timestamps are fixed under `--reproducible`; nothing
  depends on `--jobs`, thread count or the host's users.
- A failed `RUN` fails the stage with its exit status and is never retried;
  an `EXDEV` failure in overlay mode suggests rerunning with `--mode=kaniko`.
- A `RUN` that changes nothing gives an empty layer, with a test. Stage
  squashing exists only in sequential kaniko mode.

## Refs

- **Design:** [`design/scheduler.md#lazy-export`](../../design/scheduler.md#lazy-export), [`design/overlay-snapshot.md#one-layer-per-instruction`](../../design/overlay-snapshot.md#one-layer-per-instruction), [`#lazy-export`](../../design/overlay-snapshot.md#lazy-export), [`design/kaniko-mode.md#sequential-flow`](../../design/kaniko-mode.md#sequential-flow), [`#after-a-failed-run`](../../design/kaniko-mode.md#after-a-failed-run); [ARCHITECTURE.md](../../../ARCHITECTURE.md) (data flow of a build)
- **Decisions:** [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (kaniko flags), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (differences between modes), [ADR-0016](../../adr/0016-overlay-mount-options.md) and [ADR-0023](../../adr/0023-mode-flag.md) (`EXDEV` hint), [ADR-0005](../../adr/0005-own-versioned-cache-key.md) (cache keys)
- **Evidence:** [exp-08](../../research/experiments/08-parallel-stages-lazy-export.md) (lazy export saved 1.3–6.6 s), [exp-06](../../research/experiments/06-overlay-vs-walk.md) (about 2 s to export one large layer), [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) (export variance, image check), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) (lazy saved 2.8–3.6 s in parallel kaniko mode)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/commands.go:32` `DockerCommand`,
  the shape to compare `Executor` with; `pkg/executor/build.go:603`
  `stageBuilder.build`, the per-instruction loop; `:1019`
  `shouldTakeSnapshot`; `:1034` `saveSnapshotToImage`; `:1689` `DoBuild`.
- **External:** <https://github.com/opencontainers/image-spec/blob/main/config.md>, <https://docs.docker.com/reference/dockerfile/>

## Tests

- **T0:** each executor's config change (`ENV`, `LABEL`, `USER`, `WORKDIR`,
  `ENTRYPOINT`, `CMD`, `SHELL`, `EXPOSE`, `VOLUME`, `STOPSIGNAL`,
  `HEALTHCHECK`) against BuildKit and kaniko config goldens; the
  `LayerDemand` table; assembly of fake layers gives stable digests.
- **T1/T2:** W1 and W3 end to end; flattened content against kaniko
  (`nextjs-kaniko/digest-check/`) and between modes (ADR-0012 list);
  `--jobs 1` and the default give one digest. Phase 6: kaniko's 201
  integration Dockerfiles in every mode.

## Metrics

- The instruction record `rnk_builder.instruction` (`stage`, `index`,
  `line`, `kind`, `text` as written, never expanded, `cache_hit`); `stage`,
  `execute`, `layer` (`skipped`), `assemble`, `output` ([rows](../../design/metrics.md#rnk-builder)).
- Test: with `--reproducible`, digests equal with `RANIKUST_METRICS_FILE`
  set and unset.

## Pitfalls

- Exporting costs about 2 s per large layer: lazy export matters as much as
  the diff method (exp-06 finding 7, exp-08).
- A late export re-reads reclaimed pages, 1.8–6.6 s for the same layer
  (exp-07 finding 7); exporting every layer in parallel was 3 s slower at
  32 CPUs (exp-08 finding 3).
- Writing `COPY` output straight into `diff-k` skips destination symlinks;
  go through `rootfs/` (overlay-snapshot design).
- An accepted kaniko flag with a slightly different meaning is a trap; each
  needs a test of its meaning (ADR-0011).
- Retrying a failed `RUN` is unsafe: it may have written part of its output
  (kaniko-mode design).

## Phase 1

- Build, in order: `BuildPaths` and `StageCx` → `FROM` (an image or
  `<stage>`) → metadata executors (`ARG`, `ENV`, `WORKDIR`, `USER`, `LABEL`,
  `EXPOSE`, `VOLUME`, `ENTRYPOINT`, `CMD`, `SHELL`, `STOPSIGNAL`,
  `HEALTHCHECK`) → local `COPY`/`ADD` (`--chown`, `--chmod`, `--from`) →
  `RUN` (shell, exec, heredoc; `--mount=type=cache|secret`;
  `--network=none`) → lazy export → assembly and output (push, Docker tar,
  OCI layout, `--reproducible`).
- Deferred: the kaniko-mode flow with squashing and `COPY --from` file saving
  (Phase 2); cache checks (Phase 4); `ADD` URL, git and tar, `ONBUILD`,
  `COPY --link`/`--parents`/`--exclude`, `RUN --mount=type=bind` (Phase 5).
- [open: Q-029] Who computes `LayerDemand` from the graph and the outputs.
