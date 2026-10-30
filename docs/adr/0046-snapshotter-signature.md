---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer), by the rule that internal design questions take the existing recommendation
---

# ADR-0046: `Snapshotter` names its base-layer and diff types, so overlay and kaniko mode share one builder

## Context and Problem Statement

Phase 1 task 9 gave `rnk-snapshot`'s traits an interim shape (Q-024):
`Snapshotter::{base_layer, prepare}` and
`StageRootfs::{root, mount_point, begin_layer, end_layer, view, release}`,
with two overlay types in the signatures: `LayerDir`, a base layer
extracted in the store, and `LayerDiff`, an overlay upper directory.
Kaniko mode (Phase 2 task 6) has neither: its base layers are extracted
straight into the one live root, so the store keeps only their blobs, and
its diff is the walk's `WalkDiff`, which must be encoded while the root
still holds what was walked. A stage whose layers are not needed must not
be walked at all (ADR-0038, ADR-0044). `rnk-builder` runs every
instruction through these traits and should not know which mode it drives.

## Considered Options

- Keep the overlay types and add a second, kaniko-only builder path.
- Associated types: each `Snapshotter` names its base layer, each
  `StageRootfs` its diff, and both diffs implement one export trait; a
  stage root is told whether its layers are needed.
- An enum of both modes' types in `rnk-snapshot`.

## Decision Outcome

Chosen: associated types.

- `Snapshotter::Layer` (`Clone + Debug + Send + Sync`) is what `base_layer`
  returns and `Base::Image` carries: `LayerDir` for overlay mode, the blob
  (`KanikoLayer`: diff ID and store path) for kaniko mode. `base_layer`
  takes the blob's path in the store instead of a reader, so kaniko mode
  can reopen it when a stage is prepared.
- `StageRootfs::Diff: ExportLayer` is what `end_layer` returns: overlay's
  `LayerDiff`, kaniko's `KanikoDiff` (the `WalkDiff` with the root).
  `ExportLayer::export(self, options, out)` encodes it and returns the
  digests and `DiffCounts`. The builder encodes a needed diff before the
  next instruction, which kaniko mode requires and overlay mode allows.
- `StageRootfs::set_layers_needed(bool)`, called once after `prepare`:
  kaniko mode then walks or not; overlay mode ignores it.
- `Base<'a, R, L>` gains the layer type. `PulledImage<L>` carries
  `layers: Vec<L>`.
- The rest of the interim shape stays: `begin_layer`/`end_layer` around
  each instruction that changes files, `view` between layers, `release`
  consuming the root.

### Consequences

- Good, because `rnk-builder` and `ranikust`'s build stay one code path,
  generic over the snapshotter.
- Good, because kaniko mode stores no extracted copy of a base layer.
- Bad, because the generic parameters spread to `PulledImage`,
  `StageDone`, `ImageSource` and `ranikust`'s build.
- Neutral, because overlay mode behaves exactly as before.

## Evidence

- The interim shape and its one implementation: Q-024 (Phase 1 task 9,
  `feat/rnk-snapshot`); kaniko mode's needs:
  [kaniko-mode](../design/kaniko-mode.md#stage-roots-on-the-shared-root).

## More Information

Answers [Q-024](../open-questions.md#q-024-what-is-the-snapshotter-signature).
Refines [ADR-0002](0002-two-snapshot-modes.md) (the trait it names).
