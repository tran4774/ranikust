---
status: accepted
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0067: The CLI crate orchestrates the build across the library crates

## Context and Problem Statement

`crates/ranikust` states in its `# Boundaries` (`main.rs`), in
`ARCHITECTURE.md` and in its crate card that it holds "no build logic:
everything past flag parsing and process setup lives in the library
crates". Phase 2 put build orchestration in `crates/ranikust/src/build.rs`:
the stage reader counts and the hand-over decision of per-stage directories
(ADR-0061, ADR-0062), the background release thread, `kaniko_units` and
`in_target`. The end-of-Phase-2 refactor of `ranikust` found the rule and
the code disagreeing. Either the code moves into a library crate, or the
rule is reworded to what the crate does.

## Considered Options

- (a) Move the orchestration into `rnk-sched` or `rnk-builder` (an
  architecture change, its own branch).
- (b) Reword the boundary: the binary orchestrates a build across the
  library crates (which crates do what, in which order, with which shared
  state), while every rule of a single concern stays in its crate.

## Decision Outcome

(b) (maintainer, 2026-11-02: "2 b"). The code that wires the crates
together for one build — reader counts and hand-over decisions computed
from the stage graph, the release thread, the unit list, the push set —
belongs to the binary, which is the only place that sees every crate. What
the binary must still not hold is the logic of one concern: Dockerfile
semantics (`rnk-dockerfile`), scheduling (`rnk-sched`), executing an
instruction (`rnk-builder`), snapshots (`rnk-snapshot`), the sandbox
(`rnk-sandbox`), filesystem access (`rnk-rootfs`), storage (`rnk-store`),
layers, images and registries, and probing a capability.

### Consequences

- Good, because the documented boundary matches the code, and no working,
  reviewed code moves at the end of a phase.
- Good, because a rule that belongs to one crate still has one home there;
  the binary calls it (for example `rnk_dockerfile::layers_needed` for the
  push set).
- Bad, because `build.rs` stays large; a later refactor may still move a
  piece into a library when it grows a second caller.

## Evidence

- Refactor report of `ranikust` (2026-11-01) and its reviewer, host-safety
  review: the orchestration in `build.rs` is behaviour the binary alone can
  wire.
