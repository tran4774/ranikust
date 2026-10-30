---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer), by the rule to follow kaniko and BuildKit
---

# ADR-0044: Sequential kaniko mode walks every stage whose image is kept for a later `FROM`

## Context and Problem Statement

ADR-0038 walks a stage when it is the result or a walked stage builds
`FROM` it (Q-034). On the shared root a stage that is not folded into its
child (two stages build `FROM` it, or `COPY --from` also reads it) is
followed by emptying the root. A child that is not itself walked, because
only `COPY --from` reads it, must still start from that stage's root, and
after the root is emptied only the stage's layers can rebuild it (Q-300).

## Considered Options

- (a) Walk every stage whose image is kept (`save_image`), so a child
  starts from its layers, as kaniko's `SaveStage` does.
- (b) Keep a copy of the whole root in the state directory instead,
  without a walk.

## Decision Outcome

Chosen: (a). In sequential kaniko mode `layers_needed` is true for every
unit with `save_image`, in addition to ADR-0038's rule; per-stage
directories keep ADR-0038's rule unchanged (`rnk_dockerfile::layers_needed`).

### Consequences

- Good, because it is kaniko's behaviour: a stage it saves is snapshotted
  and pushed to its stage cache.
- Good, because one walk costs less than a second full copy of the root.
- Neutral, because such a stage is walked even when its own layers reach
  no output; only stages read by `COPY --from` alone are skipped.

## Evidence

- kaniko at `44aa7ddc0`: `SaveStage` set from the dependency count
  (`pkg/dockerfile/dockerfile.go:386`, `:410`); a saved stage's image is
  kept (`pkg/executor/build.go:1667`, `:2145`).

## More Information

Refines [ADR-0038](0038-kaniko-mode-settings-for-phase-2.md) (lazy
snapshots). Built in `rnk_dockerfile::squash_for_shared_root`. Design:
[kaniko-mode](../design/kaniko-mode.md#lazy-snapshots).
