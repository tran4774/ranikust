---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0006: Persistent store with prune, digest locks and atomic rename

## Context and Problem Statement

kaniko usually runs in a fresh pod and downloads and extracts every base layer
on every build. ranikust also runs on hosts and long-lived runners, where
keeping pulled blobs and extracted layers between builds saves time. A
persistent store must not grow without bound, must not be corrupted by a
crash, and must tolerate several builds using it at once
([ADR-0007](0007-run-anywhere-shared-store.md)). Should the store persist?

## Considered Options

- Ephemeral: every build starts from an empty store and deletes it at the end.
- Persistent: keep the store between builds, with pruning, locking and atomic
  writes.

## Decision Outcome

ranikust keeps a persistent store between builds, with `ranikust prune`,
per-digest locks and atomic publication by extracting to a temporary
directory and renaming it into place.

- The store keeps compressed blobs, extracted layers and `RUN --mount=type=cache`
  directories. Layers are content-addressed and immutable once published.
- `ranikust prune` removes entries by total size and by age.
- A lock per digest ensures two stages or builds never extract or download the
  same layer at the same time.
- Every entry is written under a temporary name and renamed into place, so a
  crash never leaves a half-written layer under a valid name.
- Per-build scratch space (`builds/<id>`) is not part of the persistent
  store: it is deleted after the build, and after a crash on the next start.

Layout, locking and prune rules: [cache-and-store](../design/cache-and-store.md).

### Consequences

- Good, because shared base images are downloaded and extracted once per
  machine instead of once per build.
- Good, because a crash cannot publish a corrupted layer.
- Bad, because disk usage grows until pruned, and the prune policy needs
  sensible defaults.
- Bad, because the lock protocol must be correct under crashes and
  concurrency; this was not tested in the survey.

## Evidence

- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §5:
  extracting the 5-layer `node` base in parallel per layer took 0.37 s against
  0.88 s for kaniko; with a persistent `layers/` store this cost drops to
  zero from the second build [inferred].
- [Survey 05](../research/survey/05-decisions.md) Q4;
  [survey 06](../research/survey/06-overlay-and-parallelism.md) §2 (layout);
  [survey 04](../research/survey/04-risks-and-roadmap.md) risk R10 (crash
  leftovers).

## More Information

Store concurrency and crash-in-the-middle behaviour are listed as untested in
[survey 00](../research/survey/00-summary.md) and are phase 1 and phase 6
work.

The store is owned by the `rnk-store` crate ([ADR-0021](0021-rnk-store-crate.md))
and lives in `--state-dir` in every mode ([ADR-0022](0022-common-state-dir.md)).
