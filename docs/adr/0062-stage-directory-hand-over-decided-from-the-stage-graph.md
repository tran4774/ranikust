---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0062: A stage directory is handed over only to its only reader, decided from the stage graph

## Context and Problem Statement

[ADR-0061](0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md)
hands a parent stage's directory to the last stage that reads it, decided
at run time from a count of readers still to come. The host-safety review
of its implementation found that a directory handed over is not byte for
byte the copy it replaces: ext4 and xfs never shrink a directory emptied in
place, while a copy builds compact directories, and kaniko mode's walk
compares a directory's size (`crates/rnk-snapshot/src/kaniko/walk.rs`).
With two readers (two `FROM p` children, or a child and another stage's
`COPY --from=p`), which one is last depends on scheduling; a later step
that adds an entry to such a directory and puts its time back (`touch -r`)
then puts the directory in a layer on one path only. Layer contents and
digests must never depend on scheduling (`docs/agents/rules.md`).

## Considered Options

- Decide at run time (ADR-0061 as written), and make the two paths equal
  in every attribute the walk reads, directory sizes included.
- Decide from the stage graph: hand over only when the parent has exactly
  one reader, which is then the stage built `FROM` it; copy otherwise.
- Never compare directory sizes in the walk.

Equal sizes cannot be produced in place on ext4 or xfs; ignoring sizes
changes kaniko's change detection for every build. The static rule keeps
the hand-over where ADR-0061 measured it (chains of stages, W1) and makes
the path a function of the Dockerfile and the targets.

## Decision Outcome

A stage built `FROM <stage>` takes that stage's directory over only when
it is that stage's only reader among the stages the targets need: no
other stage built `FROM` it, and no `COPY --from` or `RUN --mount from=`
on it, its own included. Otherwise it copies, whatever the order the
stages finish in. This replaces ADR-0061's run-time "last reader" choice;
the rest of ADR-0061 stands (the prune and move, the parent read no more,
the background release, which still counts readers at run time, since
when a directory is deleted changes no layer).

- `ranikust` computes, per stage, the number of readers from the stage
  graph (`stage_readers`) and gives `rnk-builder` a static `take_base`
  for each stage built `FROM` a stage with exactly one.
- The hand-over still falls back to a copy, from the pruned root, when an
  entry has a hardlink the prune did not find inside the root, or when the
  move fails before anything moved (`EINVAL` on a filesystem without
  `RENAME_NOREPLACE`, `EBUSY`, `EXDEV`). The choice then depends on the
  filesystem, not on scheduling; the sizes of directories differ only
  there, between machines, not between runs.

### Consequences

- Good, because the same Dockerfile, targets and filesystem always give
  the same layers, whatever `--jobs` and the order stages finish in.
- Good, because chains of stages, where each stage is read by the next
  only, still copy nothing (W1).
- Bad, because a stage with several readers copies for every one of them,
  as before ADR-0061, even when one of them could have taken it over.

## Evidence

- Host-safety review of ADR-0061's implementation (2026-11-01): the
  directory-size difference and the `touch -r` case.
- T0 `the_hand_over_is_a_function_of_the_stage_graph` (`ranikust`); the
  `stage-dirs` smoke case builds a parent with two readers and a large
  directory emptied down to one entry, a child adding an entry with the
  directory's time put back, at `--jobs 1` and `4`: one digest.

## More Information

- Refines [ADR-0061](0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md),
  whose choice of the "last reader" at run time it replaces; it follows
  from the rule that digests never depend on scheduling, within the
  maintainer's answer to Q-410 (option (c)).
- Design: [design/kaniko-mode.md](../design/kaniko-mode.md#per-stage-directories).
