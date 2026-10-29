---
status: proposed
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0025: Build metrics from `tracing` spans, aggregated by an opt-in `rnk-metrics` crate

## Context and Problem Statement

The maintainer's goal for ranikust's metrics is that they "give the insight
needed to improve each part": every phase of a build must be measurable,
showing where time, CPU, memory, disk and network go. The evidence behind
the design already came from such numbers: experiment 07's time breakdown
and experiment 08's memory figures were read from the `metrics.json` that
the maintainer's kaniko fork writes when `KANIKO_METRICS_FILE` is set
(commit `88b6d2b75`). ranikust needs the same from its first crate on,
without slowing builds that do not ask for it, without changing images, and
without coupling every library to a metrics crate. Survey 03 already picked
`tracing` for logs. How do the libraries report what they do, who turns it
into numbers, and in what format?

## Considered Options

- Global counters in a metrics crate that every library imports, read as
  deltas while a record is open: kaniko's design. Every library would depend
  on it, and concurrent work (parallel stages, background pushes) is charged
  to whatever record happens to be open.
- The `metrics` crate family or OpenTelemetry/OTLP from the start: an
  exporter and a collector to run before anything is measured; neither
  writes kaniko's per-command records.
- No in-process metrics; `perf`, `strace` and cgroup sampling from outside:
  cannot tell which instruction or layer the time belongs to.
- Libraries emit `tracing` spans; a separate crate, used only by the binary,
  aggregates them and writes a kaniko-shaped report.

## Decision Outcome

Libraries describe their work as `tracing` spans named `<crate>.<operation>`
with numeric fields that carry their unit; a new crate, `rnk-metrics`, used
only by the binary, aggregates them into one JSON report when
`RANIKUST_METRICS_FILE` is set.

- **Libraries never depend on `rnk-metrics`** (a dev-dependency, to check
  their spans in tests, is allowed). They emit `debug_span!`s such as
  `rnk_layer.encode` or `rnk_registry.push_blob`, one per operation (a
  layer, a blob, an instruction), never per file or tar entry. Hot loops
  keep local counters and record the totals once (`span.record("files",
  n)`). Field names carry the unit (`bytes_in`, `files`, `entries`,
  `blocks_read`, `user_ns`); `max_*` fields keep their maximum, the others
  are summed.
- **`rnk-metrics`** is a `tracing_subscriber::Layer` that aggregates spans
  per name (count, total, maximum, wall time, summed fields) and keeps the
  build → stage → instruction → snapshot nesting for per-stage,
  per-instruction and per-snapshot records, as kaniko's `stages`,
  `commands` and `snapshots`. It adds `getrusage` for the process and its
  waited-for children (`RUN` processes are children), peak RSS, and cgroup v2
  and `/proc/net/dev` counters with their deltas.
- **Report:** canonical JSON with sorted keys and
  `"schema": "ranikust.metrics/v1"`, with kaniko's section names and units
  where the meaning matches; the differences are documented. An optional
  short summary goes to stderr (`RANIKUST_METRICS_SUMMARY`).
- **Opt-in:** `RANIKUST_METRICS_FILE=<path>` now; a `--metrics-file` flag
  with the CLI (Phase 1 task 8). Whether `KANIKO_METRICS_FILE` is honoured
  too belongs to the kaniko variables question (Q-023).
- **Off means off:** without the variable neither the metrics layer nor the
  panic hook is installed, and a `debug_span!` costs tracing's
  disabled-callsite check. Logs are a separate layer with its own per-layer
  filter, so a log level never hides a metric.
- **Reliability:** the report is written at normal exit, on error, and from
  a panic hook, which in release builds (`panic = "abort"`) runs before the
  abort. A failure to write it is a warning; it never changes the exit code.
- **Determinism:** metrics never reach the image. The report is written only
  at exit, after the last snapshot and push, and no build code reads metric
  state.
- **Not now:** CPU or heap profiling, OTLP export and a Prometheus endpoint
  (Q-057, Q-058).

### Consequences

- Good, because one vocabulary serves logs and metrics, and libraries stay
  free of a metrics dependency.
- Good, because records follow span nesting: work is charged to the
  instruction and stage it ran for, also with parallel stages, where
  kaniko's counter deltas mix concurrent work.
- Good, because the bench harness that reads kaniko's reports can read
  ranikust's with a map of span names, so the two compare run for run.
- Bad, because the contract is a naming convention: a renamed span or field
  silently drops out of a record or a comparison. The table in
  `design/metrics.md` and span tests with `rnk_metrics::Capture` are the
  guard.
- Bad, because work on another thread (`spawn_blocking`, rayon, the `RUN`
  spawner thread) is charged to its stage and instruction only when its span
  names its parent explicitly.
- Bad, because an enabled span costs a registry slot and two short lock
  holds, about a microsecond; acceptable only while spans stay per
  operation, never per file.
- Neutral, because the global subscriber and the panic hook are
  process-wide; the binary installs them once, before any thread starts.

## Evidence

- kaniko fork `88b6d2b75` (`pkg/metrics/`, `cmd/executor/cmd/metrics.go`)
  [code]: opt-in by environment variable, aggregates only, per-command and
  per-snapshot records, the report written on success, error and panic of
  the main goroutine, image digests identical with metrics on and off
  (checked with `--reproducible`).
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md#time-breakdown-32-cpus-s-median):
  the kaniko side of the time breakdown came from `metrics.json`; the
  phases that mattered (diff 6.29 s, tar + gzip 2.58 s, push 0.72 s) each
  map to one span here.
- [Experiment 08](../research/experiments/08-parallel-stages-lazy-export.md#parallel--export-all-layers-memory-pressure):
  peak cgroup memory and disk re-reads during export explained the
  variance; both are fields of this report (`memory.peak`,
  `rnk_snapshot.export` `blocks_read`).
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md#performance-w3-and-w1):
  per-stage times and the memory limit decided `--parallel-stages`; stage
  records with start and wall time reproduce them.
- Measured on this branch: the static musl binary with the subscriber is
  0.96 MB (961,512 bytes); with `panic = "abort"` the panic hook writes the
  report and the process ends with `SIGABRT` (exit 134).

## More Information

- Design and the span every crate emits:
  [design/metrics.md](../design/metrics.md). Crate card:
  [rnk-metrics](../agents/crates/rnk-metrics.md).
- Refines survey 03's choice of `tracing`.
- Open: [Q-023](../open-questions.md#q-023-which-other-kaniko-flags-and-variables-are-accepted)
  (`KANIKO_METRICS_FILE`),
  [Q-057](../open-questions.md#q-057-does-ranikust-offer-cpu-and-heap-profiling)
  (profiling),
  [Q-058](../open-questions.md#q-058-which-metric-export-formats-come-after-the-json-report)
  (OTLP, Prometheus).
- Revisit when a long-running mode (a daemon, a CI service) needs live
  metrics rather than one report per build.
