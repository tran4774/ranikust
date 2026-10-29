---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0011: Accept the common kaniko flags

## Context and Problem Statement

Many pipelines call kaniko's executor with a familiar set of flags. kaniko
registers 71 command-line flags and 68 `FF_KANIKO_*` feature flags. Matching
all of them would import kaniko's history; matching none would force every
user to rewrite their pipeline. How compatible should the `ranikust build`
command line be?

## Considered Options

- Full compatibility with every kaniko flag and feature flag.
- Accept the commonly used kaniko flags with the same names and meanings.
- A new command line plus a migration guide.

## Decision Outcome

`ranikust build` accepts the commonly used kaniko executor flags with the same
names and meanings, so existing pipelines can switch with minimal edits; the
exact list is fixed in phase 1 from kaniko's README and sample pipelines.

- Candidates named in the survey: `--context`, `--dockerfile`,
  `--destination`, `--tar-path`, `--oci-layout-path`, `--no-push`, `--cache`,
  `--cache-repo`, `--build-arg`, `--target`, `--reproducible`,
  `--compression`, `--registry-certificate`, `--ignore-path`.
- `FF_KANIKO_*` feature flags are not carried over: their optimizations are
  the default design, not switches.
- ranikust-specific flags: `--workdir`, `--state-dir`, `--snapshot-mode`,
  `--jobs`, `--parallel-stages`, `--network`, `--seccomp`, `--userns`. The
  `--parallel-stages` name is a placeholder fixed together with this list
  ([ADR-0014](0014-kaniko-mode-sequential-by-default.md)).
- Subcommands: `build`, `push`, `prune`, `version`, `login`.

### Consequences

- Good, because most kaniko pipelines need only a binary swap and a few new
  flags.
- Bad, because a flag that is accepted but behaves slightly differently is a
  trap; every accepted flag needs a test of its meaning, and the user book
  needs a flag-by-flag mapping.

## Evidence

- [Survey 01](../research/survey/01-kaniko-today.md) §1: 71 CLI flags in
  `cmd/executor/cmd/root.go` and 68 `FF_KANIKO_*` feature flags [code].
- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §2: the
  proposed command line.
- [Survey 05](../research/survey/05-decisions.md) Q9.

## More Information

Name clash to resolve in phase 1: kaniko's `--snapshot-mode` takes
`full|redo|time`, while ranikust uses `--snapshot-mode` for
`auto|overlay|kaniko` ([ADR-0002](0002-two-snapshot-modes.md)). Whether
kaniko's values are accepted (for example as aliases inside kaniko mode) or
rejected with a clear message is decided with the flag list [inferred].
kaniko's `--kaniko-dir` corresponds to ranikust's `--state-dir`.

The `--snapshot-mode` clash is resolved by a new `--mode` flag
([ADR-0023](0023-mode-flag.md)); `--state-dir` defaults are in
[ADR-0022](0022-common-state-dir.md).
