---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0114: Every cache key is computed before the build, and fully cached stages are skipped

## Context and Problem Statement

kaniko by default looks up one stage at a time and stops at the first miss;
opt-in feature flags compute every key first, keep probing after a miss and skip
extracting fully cached stages. With chained keys, a hit after a miss is a layer
made on another build's lower. [#14](https://github.com/tran4774/ranikust/issues/14)
(Q-042) asks how lookahead and skipping work; nothing designs them yet. Three
more questions came up while planning Phase 4 (see
[the roadmap](../../README.md#roadmap)):

- Which stages' layers go to the cache.
  [ADR-0038](0038-kaniko-mode-settings-for-phase-2.md) encodes only the target's
  `FROM` chain and says `--cache` "may add stages". kaniko pushes a cache layer for
  every cacheable step of every stage it runs; BuildKit by default (`mode=min`)
  exports only the result's chain. W2's `pnpm install` sits in a stage read only
  through `COPY --from`.
- The key of `COPY --from=<stage>`, whose input is another stage's files.
  kaniko hashes the copied files by default and, behind a feature flag, uses the
  source stage's final key in place of those hashes; BuildKit keys by content.
- What `--materialize` means.
  [ADR-0064](0064-materialize-refused-until-the-layer-cache.md) refused it until
  the cache gives it a meaning "with a test of its meaning".

## Considered Options

The option letters are those of the questions put to the maintainer; #14's text
lists none.

Lookahead (#14):

- (a) Every key computed before the build, all lookups in parallel; each stage
  uses its hits up to its first miss; a stage whose steps all hit and whose root
  nothing that runs reads is never prepared.
- (b) (a), but hits after a miss are used too: more reuse, but a cached layer can
  sit on a lower that differs from the one it was made on.
- (c) kaniko's default: one stage at a time, no skipping.

Stages whose layers go to the cache:

- (a) Every stage the build runs (kaniko).
- (b) The target's `FROM` chain only (BuildKit's default).
- (c) (b) by default, (a) with a new flag (BuildKit's `mode=max`): a flag kaniko
  users do not know.

The key of `COPY --from=<stage>`:

- (a) The source stage's final key as the step's input, in place of the copied
  files' hashes: the source can be skipped, but a rebuilt source with a
  non-deterministic `RUN` leaves a hit holding the old files.
- (b) The copied files' content (kaniko's default, BuildKit): the source stage
  must run or be extracted first, so it is never skipped.
- (c) (a) when the source stage did not run in this build, a miss when it did.

`--materialize`:

- (a) Accept it in kaniko mode on `/`, refuse it elsewhere as changing nothing.
- (b) Accept it everywhere as "never skip the target stage": costs time where
  nobody sees the root.
- (c) Refuse it for good: kaniko users on `/` who read the root after the build
  lose it.

## Decision Outcome

Every cache key of a build is computed before any stage runs and all lookups run
at once, each stage uses its hits up to its first miss, a stage whose steps all hit
and which nothing that runs reads is never prepared, and `--cache` caches the
layers of every stage the build runs.

The maintainer's answer (2026-11-06: "Tính trước, bỏ qua stage (khuyến nghị)",
"Compute ahead, skip stages (recommended)") stood for option (a) of #14, option (a)
of the stages question, option (c) of the `COPY --from=<stage>` question and
option (a) of the `--materialize` question.

- **Lookahead (a).** The keys are computed after the context walk and the base
  manifests are resolved, and every step is looked up, one lookup per step even
  after a miss. A stage uses its hits up to its first miss; kaniko's stop rule
  keeps each layer on the lower it was made on. A stage whose steps all hit, and
  whose root nothing that runs reads, never has its base pulled into the store,
  extracted or mounted. If it is the target, its image is the base's layers and
  the cached layers, sent as base layers are
  ([ADR-0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md)).
- **Every stage (a).** A cold build walks, encodes and pushes the layers of stages
  that only a `COPY --from` reads, so builder-stage files reach the cache
  repository; the repository is trusted as the destination is. This is what
  ADR-0038 left open: under `--cache` the stages whose layers are encoded are
  every stage the build runs, not only the target's `FROM` chain. Without
  `--cache` ADR-0038's rule is unchanged.
- **`COPY --from=<stage>` (c).** When the source stage did not run in this build,
  the source stage's final key is the step's input, in place of the copied files'
  hashes; the step's key is still ADR-0005's, with `key(k-1)` and the instruction.
  When the source stage did run, the step is a miss: it copies what the stage made
  and is pushed under the key computed from that input. The key stays one computed
  before the build, so skipping works, and a rebuilt stage's files are what the
  image gets.
- **`--materialize` (a).** It returns in kaniko mode on `/`: the target's root is
  extracted onto `/` even when every step hits, as in kaniko. It is refused in
  overlay mode and in kaniko mode with a directory per stage, whose roots are
  deleted at the end, as changing nothing
  ([ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md)). It comes
  back with a test of its meaning in the mode that accepts it, as ADR-0064 asks.

### Consequences

- Good, because a warm build can run no `RUN` and prepare no stage it skips, and the
  lookups run beside the base pulls.
- Good, because a partial build reuses a builder stage such as W2's `deps`, which
  only a `COPY --from` reads.
- Good, because no cached layer sits on a lower it was not made on.
- Bad, because every step is looked up even after a miss, requests that cannot be
  used.
- Bad, because a cold build does the work of encoding and pushing every stage,
  not only the target's chain.
- Bad, because a `COPY --from` whose source did not run takes the layer cached
  under a key whose input is the source's final key, not a hash of the files: it is
  as good as the source stage's own cached layers, and no better.
- Bad, because a kaniko command line with `--materialize` is refused wherever the
  mode is not kaniko on `/`.

## Evidence

- **[external]** kaniko@44aa7ddc0 (the benchmark fork): lookup per stage stops at the
  first miss unless `FF_KANIKO_CACHE_PROBE_AFTER_MISS` (`pkg/executor/build.go:551`,
  `:567-569`); the opt-in lookahead is at `:1762-1820`; it pushes a cache layer for
  every executed step (`:904-954`); a fully cached stage skips its base unpack unless
  a step needs the files, a later stage reads it or `--materialize` is given
  (`:606-654`, `:623-625`); `FF_KANIKO_IMAGE_STAGES` also skips extracting hits
  (`:831`); `COPY --from=<stage>` hashes the copied files by default
  (`pkg/commands/copy.go:430-445`) and uses the source stage's final key behind
  `FF_KANIKO_INFER_CROSS_STAGE_CACHE_KEY` (`pkg/executor/build.go:308-320`,
  `:485-503`).
- **[external]** buildkit@cd8ee244: a registry cache export with `mode=min` exports
  the result's chain, `mode=max` every step (`solver/types.go:98-107`,
  `control/control.go:720-728`); `COPY --from` keys by content
  (`solver/llbsolver/ops/file.go:55-186`).
- **[code]** `LayerDemand::for_stage` (`crates/rnk-builder/src/layer.rs:35-70`) takes
  `cache` as an input and is always false today; W2's `deps` stage is read only
  through `COPY --from` (`nextjs-kaniko/app-cache/Dockerfile`).
- **[inferred]** Nothing is measured: W2 has never been built with a cache, so the
  saving of skipping is expected, not shown.

## More Information

Answers #14 and three Phase 4 questions (which stages are cached, the key of
`COPY --from=<stage>`, `--materialize`). Refines
[ADR-0038](0038-kaniko-mode-settings-for-phase-2.md) (which stages `--cache` adds)
and fulfils ADR-0064's "Phase 4 brings it back"; neither is rewritten. Related:
[ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)
(the key).

Not covered: a content-based `COPY --from` key beside this definition-based one
(listed under "carried over" in
[#123](https://github.com/tran4774/ranikust/issues/123)).

Follow-ups, with Phase 4's tasks and not before they are built: change
[cache-and-store](../design/cache-and-store.md), the
[scheduler](../design/scheduler.md) and [kaniko-mode](../design/kaniko-mode.md)
design docs, the `rnk-sched` and `ranikust` cards, the book's `--materialize`
entry and the kaniko map, and add the `## Changelog` entry.
