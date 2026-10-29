---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0021: A new crate, `rnk-store`, owns the persistent store

## Context and Problem Statement

[ADR-0006](0006-persistent-store.md) keeps a persistent, content-addressed
store with per-digest locks, atomic rename and `ranikust prune`;
[ADR-0007](0007-run-anywhere-shared-store.md) lets concurrent builds share
it. No crate's `# Boundaries` section assigned that store. The design gave
the extracted layers to `rnk-snapshot`, blob transfer to `rnk-registry` and
the `prune` command to the binary, and left `blobs/`, `tmp/`, the locks and
the prune logic without an owner (Phase 1 open question O1). `BuildPaths`,
the paths every crate receives, was sketched in `rnk-builder`, above the
crates that need it. Which crate owns the store?

## Considered Options

- `rnk-snapshot` owns all of it. It already held `layers/`, but
  `rnk-registry` and `rnk-cache` would depend on the snapshot crate for
  blobs, and the lock protocol would sit next to mount code.
- Split by directory: `layers/` in `rnk-snapshot`, `blobs/` in
  `rnk-registry`, prune in the binary. Three crates would implement the same
  lock, rename and clean-up protocol, and prune would have to know them all.
- `rnk-cache` owns it. The local store is not the registry layer cache
  (ADR-0006), and `rnk-cache` is a Phase 4 crate.
- A new crate, `rnk-store`, below every crate that uses the store.

## Decision Outcome

A new crate, `rnk-store`, owns the persistent content-addressed store, its
locks, its clean-up and `BuildPaths`; every other crate reaches the store
through it.

- It owns the store layout: `blobs/`, the extracted `layers/`, `caches/` for
  `RUN --mount=type=cache`, and `tmp/`.
- It owns the per-digest locks, publication by atomic rename, start-up
  clean-up and `prune`.
- It owns `BuildPaths`, the paths handed down to every crate.
- `rnk-registry`, `rnk-snapshot`, `rnk-cache` and `rnk-builder` use it. The
  `ranikust prune` subcommand only calls `rnk-store`.
- It does not decide what an entry contains: the caller fills a temporary
  entry (a downloaded blob, an extracted layer) and `rnk-store` publishes
  it. It transfers no blob, decodes no layer and mounts nothing.

### Consequences

- Good, because one crate implements the lock, rename and clean-up protocol
  that ADR-0006 and ADR-0007 rely on, and it can be tested on its own at
  tier T0.
- Good, because `prune` and start-up clean-up see the whole layout.
- Good, because `BuildPaths` sits below every crate that needs it.
- Bad, because the workspace grows to fourteen crates, with one more
  boundary to keep.
- Neutral, because the on-disk format of `layers/` is unchanged
  ([ADR-0016](0016-overlay-mount-options.md)); where the store lives is
  [ADR-0022](0022-common-state-dir.md).

## Evidence

- [Survey 03](../research/survey/03-rust-architecture.md) §2: the proposed
  workspace has no store crate; the layer store sits inside the snapshot
  crate and `pkg/config`'s globals become a `BuildPaths` value.
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §5:
  per-layer parallel extraction of the `node` base took 0.37 s against
  0.88 s in kaniko; a shared, persistent `layers/` makes this a one-time cost
  [inferred].
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md)
  finding 9: a hardlink farm let one `echo >>` change the store and a
  sibling stage; store entries must be immutable and only published whole.
- The [cache-and-store design](../design/cache-and-store.md#ownership) as of
  2026-10-29 assigned no owner to `blobs/`, `tmp/`, the locks or prune
  [code].

## More Information

Refines [ADR-0006](0006-persistent-store.md) and
[ADR-0007](0007-run-anywhere-shared-store.md) by naming the crate that
implements them. Design: [cache-and-store](../design/cache-and-store.md).
The lock protocol under crashes and races is still untested: Phase 1 task 7
and phase 6.
