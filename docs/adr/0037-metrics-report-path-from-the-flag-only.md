---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0037: The metrics report path comes only from --metrics-file

## Context and Problem Statement

`ADR-0025` turns the build metrics report on with the
`RANIKUST_METRICS_FILE` environment variable, and the CLI later added
`--metrics-file`, which took the variable's place when both were given.
`ADR-0036` then decided that kaniko's `KANIKO_METRICS_FILE` is not an alias.
Two ways to name one file meant two code paths and a precedence rule to
test and document, and a variable inherited from a CI runner's environment
could write a report nobody asked for into the workdir. Should the variable
stay?

## Considered Options

- Keep both, the flag over the variable (as built).
- The flag only (taken).
- The variable only: hides the setting from `--help` and from the command
  line a pipeline records.

## Decision Outcome

The metrics report is written only when `--metrics-file=<path>` is given;
`RANIKUST_METRICS_FILE` is no longer read.

- `--metrics-file` is a flag of every subcommand (`ranikust version
  --metrics-file=…` too), so each way out of the binary, `version`
  included, can still be measured.
- When `RANIKUST_METRICS_FILE` is set, ranikust writes no report from it
  and says so in one warning naming `--metrics-file`.
- `RANIKUST_METRICS_SUMMARY` (the stderr summary) is unchanged.
- Everything else in `ADR-0025` stays: the spans, the report's contents, its
  writing on every exit path, and metrics off meaning off.

### Consequences

- Good, because the report path has one source, visible in `--help` and in
  the command line a pipeline logs.
- Good, because an inherited environment cannot turn the report on.
- Bad, because pipelines that set the variable must switch to the flag; the
  warning tells them.

## Evidence

- The variable and the flag in the code before this change:
  `crates/rnk-metrics/src/lib.rs` (`FILE_ENV`, `Config::from_env`) and
  `crates/ranikust/src/main.rs` (`metrics_config`, the flag over the
  variable) [code].
- Tests: `ranikust` `tests/metrics.rs` (every exit path through
  `--metrics-file`, and `the_old_variable_writes_nothing_and_warns`),
  `cli::tests::metrics_file_is_a_flag_of_every_subcommand`,
  `tests/cli.rs` `the_report_path_comes_from_the_flag_only`, `rnk-metrics`
  `config_reads_the_environment_values`.

## More Information

- Supersedes the `RANIKUST_METRICS_FILE` part of `ADR-0025` only; the rest
  of `ADR-0025` is in force, so its status is unchanged.
- Related: `ADR-0036` (no `KANIKO_METRICS_FILE` alias).
- Design: [metrics](../design/metrics.md).
