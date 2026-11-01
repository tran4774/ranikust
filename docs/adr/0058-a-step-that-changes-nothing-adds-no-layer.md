---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0058: A step that changes nothing adds no layer, only its history entry, as kaniko

## Context and Problem Statement

Q-131, answered under [ADR-0032](0032-observable-behaviour-follows-buildkit.md),
follows BuildKit: `RUN`, `COPY`, `ADD` and a `WORKDIR` other than `/` each
add a layer. A step that changes nothing (`RUN true`, a `WORKDIR` of a
directory that exists, a `COPY` whose wildcard matches nothing) then adds
an empty layer, and two such steps give the same digest and diff ID
twice. BuildKit does this too, and containerd refused such an image on
GKE ("conflicting lowerdir path", moby/buildkit#6543, open). kaniko adds
no layer for a step that changed no file and records it in the history as
an empty layer. The issue mining found the case; Q-385 asked which to
follow.

## Considered Options

- (a) Keep BuildKit's empty layers (Q-131 as answered).
- (b) A step whose layer is empty adds no layer, only its history entry
  with `empty_layer: true`, as kaniko.
- (c) Keep a step's first empty layer and drop only adjacent repeats.

## Decision Outcome

Chosen: (b). The maintainer, on 2026-11-01: "Q385 làm như kaniko".

- After a `RUN`, `COPY`, `ADD` or `WORKDIR` (other than `/`), a layer
  whose tar has no entry (diff ID
  `sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef`,
  1024 zero bytes) is not added to the image and not pushed; the
  instruction's history entry stays, with `empty_layer: true`.
- Both modes, from one place (`rnk_builder` `StageBuilder::layer`): an
  overlay upper directory with nothing to export and a kaniko-mode walk
  that found no change give the same result, so the two modes build the
  same image for such a step.
- What counts is the layer as encoded: an entry the overlay exporter finds
  unchanged against the lower layers is left out, as before; a directory
  whose metadata changed is a change.
- Supersedes in part: Q-131's answer recorded in ADR-0032, as far as it
  gives a step that changes nothing a layer. The rest of that answer
  (one history entry per instruction, `empty_layer` for metadata
  instructions, none for `FROM` and `ONBUILD`, no author, the `comment`)
  stands.

### Consequences

- Good, because no image repeats a diff ID, which is the case containerd
  refused; and every runtime mounts one lower directory fewer per such
  step, which also counts against overlay's limit of lower directories.
- Good, because no empty blob is uploaded or referenced.
- Neutral for security: an empty layer holds nothing.
- Bad, because for such steps the image differs from BuildKit's (fewer
  layers, other digests); compatibility with BuildKit is not a goal in
  itself ([ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md)).
- Neutral, because the layer is still encoded before it is known to be
  empty (1024 bytes), then dropped.

## Evidence

- moby/buildkit#6543 (issue mining card `buildkit/6543`) [external]:
  repeated empty layers refused by containerd on GKE.
- kaniko: a snapshot with no changed file appends an empty layer to the
  config history and adds no layer [external].
- `crates/rnk-builder/src/stage.rs` (`StageBuilder::layer`,
  `EMPTY_LAYER_DIFF_ID`) [code]; tests
  `crates/rnk-builder/tests/keep.rs`
  `a_step_that_changes_nothing_adds_no_layer` (kaniko-mode walk: layer
  count, history, the same digests twice) and the quick smoke cases `run`
  and `hygiene` (both modes, `smoke_check_no_empty_layer`) [measured].

## More Information

Answers [Q-385](../open-questions.md). Partly supersedes ADR-0032's answer
to Q-131; ADR-0032's body is unchanged. Priority rule: ADR-0049.
