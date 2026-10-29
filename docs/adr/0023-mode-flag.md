---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0023: `--mode` selects the snapshot mode; `--snapshot-mode` keeps kaniko's meaning

## Context and Problem Statement

[ADR-0002](0002-two-snapshot-modes.md) named the mode selector
`--snapshot-mode=auto|overlay|kaniko`.
[ADR-0011](0011-accept-common-kaniko-flags.md) accepts kaniko's common flags
with the same names and meanings, and kaniko already has
`--snapshot-mode=full|redo|time`, default `full`, which picks what its walk
compares for each file. The two meanings cannot share one flag without
breaking either ADR-0011's promise or ADR-0002's selector (Phase 1 open
question O2). What is the mode flag called, and what happens to kaniko's?

## Considered Options

- Keep `--snapshot-mode` for `auto|overlay|kaniko` and accept `full`,
  `redo` and `time` as aliases inside kaniko mode: one flag with two value
  sets that mean different things.
- Keep ranikust's meaning and reject kaniko's values with a message:
  pipelines that pass `--snapshot-mode=redo` or `time` break.
- A new flag for the mode; `--snapshot-mode` keeps kaniko's meaning.

## Decision Outcome

The snapshot mode is selected with a new flag, `--mode=auto|overlay|kaniko`
(default `auto`), and `--snapshot-mode=full|redo|time` keeps kaniko's
meaning: the walk strategy in kaniko mode.

- `--mode=auto` runs the probe and picks a mode. `--mode=overlay` and
  `--mode=kaniko` skip the choice but not the checks: a forced mode that
  cannot work fails with the probe's diagnosis.
- `--snapshot-mode` (default `full`) selects what the kaniko-mode walk
  compares, as kaniko does: `full` content and metadata, `redo` mode, mtime,
  size, uid and gid, `time` mtime only.
- In overlay mode `--snapshot-mode` is accepted and ignored with a warning:
  the upper directory is the diff, and nothing is compared.
- The `EXDEV` hint of [ADR-0016](0016-overlay-mount-options.md) becomes
  "rerun with `--mode=kaniko`".

### Consequences

- Good, because every kaniko flag that ranikust accepts keeps its meaning,
  including `--snapshot-mode`.
- Good, because the mode selector has a short name of its own.
- Bad, because `--mode` and `--snapshot-mode` are easy to confuse; the
  warning in overlay mode and the user book must keep them apart.
- Bad, because kaniko mode must implement and test three walk strategies
  instead of one (Phase 2).

## Evidence

- kaniko: `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:341` (`--snapshot-mode`,
  default `full`), `pkg/executor/build.go:2474` (`getHasher`: `time`,
  `full`, `redo`, anything else an error), `pkg/util/util.go:52` (`Hasher`,
  content and metadata), `:164` (`MtimeHasher`), `:180` (`RedoHasher`: mode,
  mtime, size, uid, gid) [code].
- [Survey 05](../research/survey/05-decisions.md) Q9 and
  [ADR-0011](0011-accept-common-kaniko-flags.md) More Information: the clash
  was left to the Phase 1 flag list.
- [Experiment 10](../research/experiments/10-exdev-package-managers.md):
  `EXDEV` hit only npm and pip among the package managers tested, and both
  fell back to copying; the hint is for the tools that do not.

## More Information

Refines [ADR-0002](0002-two-snapshot-modes.md) (the selector's name),
[ADR-0004](0004-support-every-privilege-level.md) (forcing a mode),
[ADR-0011](0011-accept-common-kaniko-flags.md) (resolves its name clash)
and [ADR-0016](0016-overlay-mount-options.md) (the hint). Probe order:
[modes-and-detection](../design/modes-and-detection.md). The final flag
list, including the `--parallel-stages` name
([ADR-0014](0014-kaniko-mode-sequential-by-default.md)), is still fixed in
Phase 1.
