# rnk-metrics

> **Status:** in progress · **Phase:** 1 ·
> **Depends on:** `tracing`, `tracing-subscriber`, `serde_json`, `libc` ·
> **Used by:** `ranikust` only · **Path:** `crates/rnk-metrics/`

**TL;DR:** The process's `tracing` subscriber: logs to stderr and, when a
report file is asked for (`--metrics-file`, ADR-0037), a layer that
aggregates every crate's spans into one kaniko-shaped JSON report at exit.
Off means off; never per file; never reaches the image.

## Owns

- The global subscriber: the log layer and, when enabled, the metrics layer,
  each with its own per-layer filter.
- Aggregating spans of `rnk_*`/`ranikust` targets per name, per record
  (stage, instruction, snapshot) and per registry host
  ([records](../../design/metrics.md#records)).
- Process counters at start and exit, with deltas: `getrusage` (self and
  children), `/proc/self/status`, cgroup v2, `/proc/net/dev`.
- The report (`ranikust.metrics/v1`, canonical JSON), its atomic write, the
  human summary and the panic hook that writes it before an abort.
- `Capture`, the test helper other crates use to check their spans.
- Escaping what reaches the terminal: `terminal_text` for the colored log
  format and `ranikust`'s fatal error line, `\uXXXX` for DEL, C1, U+2028,
  U+2029 and bidi controls in the text and JSON formats
  ([turning it on](../../design/metrics.md#turning-it-on)).

## Must not

Boundaries: `crates/rnk-metrics/src/lib.rs`. In short:

- Be a dependency of a library crate (a dev-dependency for `Capture` is
  fine); libraries only emit spans.
- Influence a build: write anything but the report, or let a library read
  metric state.
- Start a thread, or keep state per file or tar entry.
- Parse the logging flags: `ranikust` maps `--verbosity`, `--log-format` and
  `--log-timestamp` into `Config::logs` (`LogOptions`, ADR-0036); this crate
  only applies them.

## API

```rust
pub const SUMMARY_ENV: &str = "RANIKUST_METRICS_SUMMARY"; // [design]
pub const SCHEMA: &str = "ranikust.metrics/v1";            // [decided]
pub struct Config { pub file: Option<PathBuf>, pub summary: bool, pub logs: LogOptions,
                    pub write_file: Option<WriteFile> }  // from_env(), is_enabled()
pub struct WriteFile(pub Arc<WriteFn>);  // a closure; None: temporary file + rename
pub fn init(config: Config) -> Telemetry;                  // once, first thing in main, single-threaded
pub struct Telemetry;  // is_enabled(), set_meta(key, impl Into<Value>), finish(&Outcome): first call writes
pub struct Outcome;    // success() | error(code, message); panics are reported by the hook
pub struct Capture;    // new(), run(f), dispatch(), report() -> Value: span sections only
pub fn terminal_text(text: &str) -> Cow<'_, str>; // controls, U+2028/9 and bidi controls escaped as \n, \u{1b}
```

The binary fills `Config::file` from `--metrics-file` after `from_env`, and
`Config::write_file` with a writer that reaches the directory through
`rnk_rootfs::OutputDir`: it keeps out of the build context and follows no
symlink (ADR-0076).

## Invariants

- Nothing a log line or the fatal error line carries can drive the
  terminal or start a line: the formatter's own colors are the only
  escape sequences written; libraries' error text stays as it is.
- Disabled: no metrics layer, no panic hook, no file; the global level hint
  stays the log layer's `INFO`, so `debug_span!` costs one comparison.
- The exit code never depends on the report; a write failure is a warning.
- Canonical bytes: sorted keys whether or not some crate enables
  `serde_json/preserve_order`; integers only.
- The panic hook never blocks on the state lock (100 ms, then
  `spans_unavailable`). One report is written at a time; a panic report
  never overwrites the normal exit's, and the temporary file is `O_EXCL`.
- Records follow span nesting, not time: a span counts in the records it is
  nested in, across threads only with an explicit parent.

## Refs

- **Design:** [design/metrics.md](../../design/metrics.md) (conventions,
  [spans per crate](../../design/metrics.md#spans-per-crate),
  [report](../../design/metrics.md#the-report),
  [comparing with kaniko](../../design/metrics.md#comparing-with-kaniko))
- **Decisions:** [ADR-0025](../../adr/0025-build-metrics-from-tracing-spans.md)
- **Evidence:** [exp-07](../../research/experiments/07-kaniko-vs-overlay.md#time-breakdown-32-cpus-s-median)
  (breakdown read from kaniko's report), [exp-08](../../research/experiments/08-parallel-stages-lazy-export.md#parallel--export-all-layers-memory-pressure)
  (peak memory, disk re-reads)
- **kaniko:** `kaniko@88b6d2b75 pkg/metrics/metrics.go` (records, stats),
  `report.go:47-232` (finish, report, sampling), `:256-398` (rusage,
  cgroup, netdev), `cmd/executor/main.go:28-52` (exit and panic paths)
- **External:** <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/layer/#per-layer-filtering>,
  <https://docs.kernel.org/admin-guide/cgroup-v2.html>

## Tests

All T0.

- Unit (`src/tests.rs`): aggregation with synthetic times (sum, max, wall
  union, nested records, hosts), real spans nested and from 8 threads,
  parallel stages, open spans and lingering children as `incomplete`, exact
  canonical bytes, missing `/proc` and cgroup files, write-once and panic
  rules.
- One process each: `tests/init_disabled.rs` (nothing installed, spans
  disabled), `tests/init_enabled.rs` (spans at every level, panic hook). The
  binary's exit paths: `crates/ranikust/tests/metrics.rs`.

## Metrics

- Emits no span; it is the collector. Every other card links its rows in
  [design/metrics.md](../../design/metrics.md#spans-per-crate).
- A new record span or reserved field name changes `state.rs` and the design
  doc together; each crate instruments itself per that table and checks its
  spans with `Capture`.

## Pitfalls

- Callsite interest is process-wide: unit tests that compare subscribers
  in one process are flaky; such checks live in their own test binary.
- `Option<Layer>::None` still answers `Interest::always()`: the disabled
  subscriber is built without the metrics layer, not with `None`.
- Spans of dependencies (h2, hyper) are per frame: the layer's filter takes
  only `rnk_*` and `ranikust` targets.
- `tracing`'s `max_level_*`/`release_max_level_*` features would compile
  metric spans out: never enable them.
- `rustix` 1.1 has no `getrusage`; it goes through `libc` with a `SAFETY`
  comment.
- A span closes only after its children, and the registry closes a parent
  through the *current* subscriber: a lingering child stretches a record;
  in tests, drop children inside `Capture::run`.
- `eprintln!` panics on a closed or full stderr, and a panic aborts: write
  to stderr through `to_stderr`, which drops errors.
