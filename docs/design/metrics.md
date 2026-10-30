# Build metrics

How ranikust measures a build: the spans every crate emits, how
`rnk-metrics` turns them into one JSON report, and how that report compares
with kaniko's `metrics.json`. The goal is that the numbers show where each
part of a build spends time, CPU, memory, disk and network, so each part can
be improved.

Decision: [ADR-0025](../adr/0025-build-metrics-from-tracing-spans.md).
Code: `crates/rnk-metrics`, wired in `crates/ranikust/src/main.rs`.

## Invariants

- **Opt-in, and off means off.** Without `--metrics-file` (or
  `RANIKUST_METRICS_SUMMARY`) the metrics layer and the panic hook are not
  installed; a `debug_span!` is rejected by tracing's level check, one
  comparison.
- **Aggregates only.** A span covers one operation: a layer, a blob, an
  instruction, a stage. Never a span or a timer per file or tar entry; loops
  count locally and record totals once.
- **Libraries never depend on `rnk-metrics`.** They emit `tracing` spans;
  only the binary installs the subscriber. A dev-dependency to check spans
  in tests is allowed.
- **Metrics never reach the image.** The report is written at exit only, and
  no build code reads metric state. Layer and config bytes are the same with
  metrics on and off.
- **The report never changes the exit code.** Failing to write it is a
  warning on stderr.
- **No thread of its own.** `init` runs while the process must still be
  single-threaded (before `unshare`, the probes and tokio).

## Turning it on

| Variable | Effect |
|---|---|
| `--metrics-file=<path>` (a flag, `ADR-0037`) | Collect, and write the report there at exit. A relative path is resolved at start-up; the parent directory is created; the file is written to a temporary name and renamed. |
| `RANIKUST_METRICS_SUMMARY=1` | Collect, and print a short summary to stderr at exit (anything but empty or `0`). |

The report path comes only from `--metrics-file`, a flag of every
subcommand; `RANIKUST_METRICS_FILE` is no longer read, and when it is set
ranikust says so in a warning ([ADR-0037](../adr/0037-metrics-report-path-from-the-flag-only.md)). kaniko's `KANIKO_METRICS_FILE` is not honoured: it
is named in the start-up warning about unread `KANIKO_*` variables
([ADR-0036](../adr/0036-kaniko-flags-in-phase-1.md)). The log level and
format (`--verbosity`, `--log-format`, `--log-timestamp`) apply to the log
layer only and never change the report.

Logs are a separate layer: stderr, at kaniko's `--verbosity` and written as
kaniko's `--log-format` and `--log-timestamp` write it, other crates at
`INFO` at most ([ADR-0036](../adr/0036-kaniko-flags-in-phase-1.md)). Each
layer has its own filter, so the log level never hides a metric span.

## Emitting spans

The rules every crate follows; `rules.md` repeats the short form.

- **Name:** `<crate>.<operation>` with the crate's underscore name:
  `rnk_layer.encode`, `rnk_registry.push_blob`. The table under
  [Spans per crate](#spans-per-crate) is the list; a new span is added
  there in the same change.
- **Level:** `debug_span!` (or `trace_span!`). An `info_span!` would be
  created for the log layer even with metrics off.
- **Granularity:** one span per operation whose count grows with layers,
  blobs, instructions or stages, never with files or entries. Budget: a few
  thousand spans per build.
- **Lifetime:** a span measures from creation to close. Create it when the
  operation starts and drop it when it ends; with async code, attach it with
  `.instrument(span)`. tracing closes a span only once its children have
  closed, so a child that lingers stretches its parent.
- **Other threads:** a span created on another thread (`spawn_blocking`,
  rayon, the `RUN` spawner thread) names its parent (`parent: &span`) or
  runs inside `span.in_scope`. Otherwise it counts only in the global
  totals, not in its stage or instruction.
- **Background work** that outlives the instruction or stage that started
  it (cache pushes) takes `rnk_sched.run` as parent, never a record span:
  under a record it would keep the record open until it ends.
- **Fork:** no span or event between `fork` and `exec` in a child.
- **Fields:** integers, declared at creation (`files = Empty` when recorded
  later) and recorded once with the total: `span.record("files", n)`.
  Recording again replaces the value.

### Field names and units

| Form | Meaning | Aggregated as |
|---|---|---|
| `bytes`, `bytes_*` | bytes: `bytes` alone is regular-file content handled; `bytes_in`/`bytes_out` what a transform read and produced (tar in, gzip out); `bytes_read`/`bytes_written` disk I/O | sum |
| plural nouns: `files`, `entries`, `whiteouts`, `requests`, `retries`, `layers`, `stages` | counts | sum |
| `blocks_read`, `inblock`, `oublock` | 512-byte blocks of disk I/O (`getrusage`) | sum |
| `*_ns` | nanoseconds of something that is not the span's own time (`user_ns`, `sys_ns` of a child) | sum |
| `*_kb` | KiB, as Linux reports RSS | sum, or maximum with `max_` |
| `max_*` | a maximum: `max_threads`, `max_rss_kb`, `max_running` | maximum |
| boolean (`hit`, `failed`, `contended`) | a flag | count of spans where it is true |

Reserved, never summed: `stage`, `index`, `line` (numbers); `kind`, `name`,
`text`, `base`, `host` (strings; other strings are ignored); `cache_hit`
(also counted). A field must not be named `count`, `total_ns`, `max_ns`,
`wall_ns` or `open`. Floats are ignored.

`text` is the instruction as written in the Dockerfile, before `ARG`
expansion; never expanded values, build arguments or secrets.

### Records

Four span names open a record, the per-instance view kaniko calls stages,
commands and snapshots. Every span closed inside a record is added to that
record's `phases`, and to every enclosing record's. A stage record opened
inside another stage's instruction (a stage built for `COPY --from`) starts
a new nesting: its spans are not charged to that instruction or stage.

| Span | Record | Identity fields |
|---|---|---|
| `rnk_sched.stage` | `stages[]`, kind `stage` | `stage` (index), `name`, `base` (written to `text`) |
| `rnk_builder.instruction` | `commands[]`, kind = `kind` field (`RUN`, `COPY`) | `stage` (else inherited), `index`, `line`, `kind`, `text`, `cache_hit` |
| `rnk_snapshot.diff` | `snapshots[]`, kind `command` | inherited from the instruction |
| `rnk_snapshot.baseline` | `snapshots[]`, kind `init` | inherited from the stage |

A `host` field on a span adds its time and fields to
`network.hosts.<host>`, once: spans nested in one that has a `host` are not
counted again.

## Spans per crate

The instrumentation contract. "Why" names the question the span answers and
the experiment whose numbers it lets us reproduce. Spans of later phases are
listed so the names are settled; each crate adds its rows when it gets the
code. All names are `[design]`.

### `ranikust`

| Span | Fields | Why |
|---|---|---|
| `ranikust.run` | | The whole process, already emitted. Parent of everything; `meta.wall_ns` adds metrics set-up. |
| `ranikust.build` | | The `build` subcommand, emitted from flag validation on: kaniko's `build.total` plus `push.total`. |
| `ranikust.workdir` | | Kaniko mode's workdir checks and lock (`ADR-0038`): canonicalize, the system-directory comparison, `flock`, the marker. |
| `ranikust.setup` | | Mount namespace, state-directory check, store open, probes, spawner start: what a build pays before its first stage. |
| `ranikust.probe` | `steps` | The probe sequence at every start, inside `ranikust.setup`; `steps` counts the crates' probes that really ran (2: the sandbox's, in forked children, and the overlay probe, in the build's namespace); `exp-03`, `exp-13`. |

### `rnk-dockerfile`

| Span | Fields | Why |
|---|---|---|
| `rnk_dockerfile.parse` | `bytes_in`, `instructions`, `stages` | Parse time, expected negligible: confirms it on kaniko's 200 Dockerfiles. |

### `rnk-ignore`

No span: matching is per path. Its cost is charged to `rnk_context.ignore`.

### `rnk-context`

| Span | Fields | Why |
|---|---|---|
| `rnk_context.load` | `files`, `dirs`, `bytes`, `files_ignored` | Context size and read time; kaniko's `context.fetch`. |
| `rnk_context.ignore` | `patterns`, `files_ignored` | Compiling `.dockerignore` and filtering, when it is a pass of its own. Inside the walk it is not timed (no per-file timer); `files_ignored` goes on `load`. |
| `rnk_context.hash` | `files_hashed`, `bytes_hashed` | Phase 4: hashing `COPY` sources for cache keys; kaniko's `cache.key_hash`. |

### `rnk-oci`

No span: pure data. Serializing configs and manifests is charged to
`rnk_builder.assemble`.

### `rnk-registry`

| Span | Fields | Why |
|---|---|---|
| `rnk_registry.auth` | `host`, `requests` | The `/v2/` probe when a registry is first used, and token round trips per registry. |
| `rnk_registry.pull_manifest` | `host`, `requests`, `retries`, `bytes_in` | Resolving base images (all up front). |
| `rnk_registry.pull_blob` | `host`, `requests`, `retries`, `bytes_in` | One blob into the store: bytes per host, retries; the pull part of `exp-07`'s base pull + extract (0.88 s kaniko, 0.37 s overlay). |
| `rnk_registry.push_blob` | `host`, `requests`, `retries`, `bytes_out`, `mounted`, `existed` | One blob: uploaded, cross-repository mounted, or already there; `exp-07`/`exp-08` push (0.72 s on W1). |
| `rnk_registry.push_manifest` | `host`, `requests`, `bytes_out` | The manifest PUT. |

`requests` counts HTTP requests including retries; `retries` the re-sent
ones. No span per HTTP request or body chunk. `requests` is the count of
the requests each `oci-client` call makes; token refreshes `oci-client`
makes on its own are not seen. `bytes_in`/`bytes_out` include retried
attempts.

### `rnk-rootfs`

| Span | Fields | Why |
|---|---|---|
| `rnk_rootfs.extract` | `entries`, `files`, `whiteouts`, `hardlinks`, `bytes_in`, `bytes_written` | One layer into a directory: kaniko's `layer.extract` (bytes = tar content, items = entries). |
| `rnk_rootfs.copy` | `files`, `dirs`, `bytes` | One `COPY`/`ADD` source set into a root. |
| `rnk_rootfs.delete` | `entries` | One safe deletion of a tree: kaniko's `fs.delete`; `exp-09`. |

### `rnk-store`

| Span | Fields | Why |
|---|---|---|
| `rnk_store.lock` | `contended` | A per-digest lock that may block; total time is lock wait; concurrent builds and stages sharing the store (`ADR-0007`). |
| `rnk_store.fill` | `reused`, `bytes_written`, `bytes_reused` | One entry: published by this build or found ready (`bytes_reused` from its recorded size, else 0); what the store saves (`ADR-0006`). |
| `rnk_store.cleanup` | `entries_removed` | Start-up clean-up. |
| `rnk_store.prune` | `entries_removed`, `bytes_freed` | Phase 5. |

### `rnk-sandbox`

| Span | Fields | Why |
|---|---|---|
| `rnk_sandbox.run` | `user_ns`, `sys_ns`, `max_rss_kb`, `minflt`, `majflt`, `inblock`, `oublock`, `nvcsw`, `nivcsw`, `failed` | One `RUN` process from fork to reap; rusage from `wait4` (the child and the descendants it waited for). Becomes kaniko's `run.child`, `child_*` and `overhead_ns`; `exp-02` sandbox overhead, `exp-15` peak memory of JVM and Next.js builds. |
| `rnk_sandbox.setup` | `stubs` | Parent-side preparation before the fork: stub mount points, journal. |
| `rnk_sandbox.probe` | `ok` | One probe step (userns, `proc`, `pivot_root`, `sysfs`); start-up cost. |

### `rnk-snapshot`

| Span | Fields | Why |
|---|---|---|
| `rnk_snapshot.prepare` | `layers` | A stage root: overlay mount over the store, or extraction plus baseline (extraction nests `rnk_layer.decode` and `rnk_rootfs.extract`). |
| `rnk_snapshot.mount` | `lowers` | One instruction's overlay mount; with `umount`, the cost overlay adds per instruction (`exp-06`; mount reuse, `Q-245`). |
| `rnk_snapshot.umount` | | One instruction's unmount. |
| `rnk_snapshot.diff` | `files_added`, `whiteouts`, `opaque_dirs` (always 0 since ADR-0034), `bytes`, `unchanged`, `bytes_compared` | Record: one instruction's diff; kaniko's `snapshot.command`. `exp-06`: 0.51–0.58 s overlay against 2.3–3.7 s walk for five steps. |
| `rnk_snapshot.baseline` | `entries` | Record, Phase 2 (built, `Walker::baseline`): the walk a kaniko-mode stage starts from; kaniko's `snapshot.init` (0.36 s on W1, `exp-07`). |
| `rnk_snapshot.walk` | `entries`, `files_hashed`, `bytes_hashed`, `stat_cache_hits`, `max_threads` | Phase 2 (built, `Walker`): one walk with its hashing (not separable without per-file timers); kaniko's `snapshot.walk` + `snapshot.hash` (3.18 s, `exp-07`). |
| `rnk_snapshot.resolve` | `paths` | Phase 2: path resolution; kaniko's `snapshot.resolve` (2.48 s, `exp-07`). |
| `rnk_snapshot.syncfs` | | Phase 2: kaniko's `snapshot.syncfs` (0.48 s, `exp-07`). |
| `rnk_snapshot.export` | `entries`, `bytes_read`, `blocks_read` | One diff fed to `rnk-layer`. `blocks_read` is the `RUSAGE_THREAD` `ru_inblock` delta of the reading thread: the disk re-reads `exp-07` and `exp-08` found under memory pressure (183–538 MB per layer). Omitted if reads fan out to a pool. Not recorded yet: the overlay exporter records `entries` and `bytes_read`. |
| `rnk_snapshot.release` | `entries` | Deleting diff directories or a stage root; kaniko's `fs.delete`. |
| `rnk_snapshot.probe` | `ok`, `api` | One overlay mount attempt of the probe: `api` is `LowerdirPlus` or `Mount2`, tried in that order. |

### `rnk-layer`

| Span | Fields | Why |
|---|---|---|
| `rnk_layer.encode` | `entries`, `parents_added`, `bytes_in`, `bytes_out`, `max_threads` | One layer: tar, compression and digests. `bytes_in` is tar bytes, `bytes_out` compressed bytes (kaniko's `layer.compress` bytes); the ratio and throughput of parallel gzip and zstd (`exp-05` Q3); tar + gzip on the critical path (2.58 s kaniko, 0.77–3.95 s overlay, `exp-07`). Compression threads do not open spans. |
| `rnk_layer.decode` | `bytes_in`, `bytes_out` | One pulled blob decompressed (usually nested in `rnk_rootfs.extract`'s caller). |

### `rnk-sched`

| Span | Fields | Why |
|---|---|---|
| `rnk_sched.run` | `stages`, `jobs`, `max_running` | The whole DAG: kaniko's `build.total`; `max_running` stages at once. |
| `rnk_sched.stage` | `stage`, `name`, `base`, `max_memory_bytes` | Record: one stage from its task's start to its end. Start and wall time per stage give `exp-12`'s per-stage parallel times; `max_memory_bytes` is the highest `memory.current` the memory gate saw. |
| `rnk_sched.wait_deps` | | Waiting for dependency stages. |
| `rnk_sched.wait_memory` | | Blocked by the memory gate (`Q-030`): memory, not CPU, bound W3 (`exp-08`, `exp-12`). |
| `rnk_sched.wait_jobs` | | Waiting for a `--jobs` `RUN` permit, inside the instruction. |

Stage wait = `rnk_sched.stage` − `rnk_builder.stage`, per stage record.

### `rnk-builder`

| Span | Fields | Why |
|---|---|---|
| `rnk_builder.stage` | `instructions`, `layers` | Building one stage once scheduled. |
| `rnk_builder.instruction` | `stage`, `index`, `line`, `kind`, `text`, `cache_hit` | Record: per-instruction totals, kaniko's `commands[]`. |
| `rnk_builder.execute` | | The executor itself; kaniko's `command.exec`. |
| `rnk_builder.layer` | `skipped` | One layer's lazy-export decision and encoding (`rnk_layer.encode` nested); kaniko's `command.layer`. Lazy export saved 1.3–6.6 s on W3 (`exp-08`). |
| `rnk_builder.assemble` | `bytes_out` | Config and manifest, their sizes; kaniko's `stage.finalize`. |
| `rnk_builder.output` | `layers`, `bytes_out` | One per OCI layout, Docker tar and push destination; `bytes_out` is what was written to disk, or the blobs uploaded (not mounted or already there) plus the manifest; kaniko's `push.total`. |

### `rnk-cache`

| Span | Fields | Why |
|---|---|---|
| `rnk_cache.key` | `files_hashed`, `bytes_hashed` | Phase 4: kaniko's `cache.key`. |
| `rnk_cache.lookup` | `hit` | Phase 4: misses = `count` − `hit`; kaniko's `cache.lookup_hit`/`_miss`. |
| `rnk_cache.push` | `bytes_out` | Phase 4: background push, parented to `rnk_sched.run`; kaniko's `cache.push`. |

## The report

`"schema": "ranikust.metrics/v1"`, canonical JSON: keys sorted, two-space
indent, final newline. Times in nanoseconds; integers only.

| Section | Holds |
|---|---|
| `schema` | `ranikust.metrics/v1`; bumped on any incompatible change |
| `meta` | `ranikust_version`, `os`, `arch`, `num_cpu`, `pid`, `file`, `start`, `end` (RFC 3339 UTC), `wall_ns`; for `build`, `mode` (the `--mode` asked for) and `options`: `compression`, `jobs`, `mode`, `network`, `no_push`, `outputs` (`destinations` count, `oci_layout` and `tar` booleans), `reproducible`, `snapshot_mode`, `targets` count (`Telemetry::set_meta`; never a destination, path, build argument, secret or credential) |
| `exit` | `status` (`success`, `error`, `panic`), `code`, `message` (500 characters at most); `panics` when an unwinding build panicked earlier; `spans_unavailable` when the panic hook could not read the span state |
| `phases` | per span name: `count`, `total_ns` (sum), `max_ns`, `wall_ns` (time at least one was open), `open` (still open at exit), and the summed fields |
| `stages`, `commands`, `snapshots` | records: `build` (always 0), `stage`, `index`, `line`, `kind`, `name`, `text`, `cache_hit`, `start_ns` (from process start), `wall_ns`, `incomplete` (open at exit), `values` (the record span's own fields), `phases` (spans nested in it: `count`, `total_ns`, `max_ns`, fields) |
| `network.hosts.<host>` | per registry host: `count`, `total_ns`, `max_ns` and the summed fields (`requests`, `retries`, `bytes_in`, `bytes_out`) |
| `resources` | `rusage_self`, `rusage_children` (`user_ns`, `sys_ns`, `maxrss_kb`, `minflt`, `majflt`, `inblock`, `oublock`, `nvcsw`, `nivcsw`); `proc_status` (`VmHWM_kb`, `VmRSS_kb`, `VmPeak_kb`, `VmSize_kb`, `Threads`); `start` and `end` samples (`cgroup`: `dir`, `tried`, `values`, `errors`; `netdev`); `delta`: `end − start` of `cpu.stat`, `io.stat`, `memory.events` and `netdev` |

A command with `rnk_sandbox.run` spans also gets kaniko's values:
`child_count`, `child_wall_ns`, `child_user_ns`, `child_sys_ns`,
`child_maxrss_kb`, `child_minflt`, `child_majflt`, `child_inblock`,
`child_oublock`, `child_nvcsw`, `child_nivcsw`, and `overhead_ns` =
`wall_ns − child_wall_ns`: what ranikust spent around the process.

Reading cgroup numbers, as for kaniko: `memory.peak` includes page cache
and covers the cgroup's whole life (kernel ≥ 5.19); `memory.stat` `anon`
and `VmHWM_kb` separate real memory. Missing files land in `errors`, never
fail the report.

## Comparing with kaniko

The bench harness (`nextjs-kaniko/bench/summarize.py`, `compare-*.py`)
reads kaniko's `metrics.json` (`kaniko@88b6d2b75 pkg/metrics/report.go`).
ranikust keeps its section names, record fields and units, so a script
needs only a map of phase names and named fields in place of `bytes` and
`items`.

| kaniko phase | ranikust span | Note |
|---|---|---|
| `build.total` | `rnk_sched.run` | |
| `push.total` | `rnk_builder.output` | |
| `context.fetch` | `rnk_context.load` | |
| `base.unpack` | `rnk_snapshot.prepare` | |
| `layer.extract` (bytes, items) | `rnk_rootfs.extract` (`bytes_in`, `entries`) | |
| `snapshot.init` | `rnk_snapshot.baseline` | kaniko mode only; overlay mode has none |
| `command.exec` | `rnk_builder.execute` | |
| `run.child` | `rnk_sandbox.run` | ranikust's includes the sandbox set-up in the child |
| `snapshot.command` (bytes, items) | `rnk_snapshot.diff` (`bytes`, `files_added` + `whiteouts`) | kaniko's bytes are the tar size |
| `snapshot.scan_full`, `snapshot.walk`, `snapshot.hash` | `rnk_snapshot.walk` (`entries`, `files_hashed`, `bytes_hashed`) | one span: hashing happens inside the walk |
| `snapshot.resolve`, `snapshot.syncfs` | `rnk_snapshot.resolve`, `rnk_snapshot.syncfs` | |
| `snapshot.tar` | inside `rnk_layer.encode` | tar and compression stream into each other |
| `command.layer` | `rnk_builder.layer` | |
| `layer.compress` (bytes) | `rnk_layer.encode` (`bytes_out`) | |
| `stage.finalize` | `rnk_builder.assemble` | |
| `fs.delete` | `rnk_snapshot.release`, `rnk_rootfs.delete` | |
| `cache.key`, `cache.lookup_hit`/`_miss`, `cache.push` | `rnk_cache.key`, `rnk_cache.lookup` (`hit`), `rnk_cache.push` | Phase 4 |

kaniko at `44aa7ddc0` adds phases for its optimization flags
(`base.reuse_*`, `snapshot.hash_cache_*`, `snapshot.tar_*`,
`layer.gzip_blocks`, `layer.extract_*`, `kaniko@44aa7ddc0
pkg/metrics/stats.go:44-126`). Their counterparts here are fields
(`stat_cache_hits` on `rnk_snapshot.walk`) or do not apply.

Differences that change how numbers read:

- **Attribution.** kaniko charges a record with the change of global
  counters while it was open, so concurrent work lands in whatever record is
  open, and concurrent stats are left out of records. ranikust charges a
  span to the records it is nested in: exact with parallel stages, but only
  for spans that name their parent across threads.
- **Wall time.** `phases.*.wall_ns` (union of intervals) is new; with
  parallel work `total_ns` exceeds it. Records' `phases` carry `max_ns`,
  kaniko's do not.
- **Snapshots.** No `context` kind (no `--preserve-context`); no `tar_bytes`:
  an overlay diff is not a tar, and the layer is encoded later, lazily
  (`rnk_layer.encode` in the same command or stage). `values` hold
  `files_added` and `whiteouts` like kaniko's.
- **Commands.** No `child_exit_code`: a failing `RUN` fails the build and
  `exit.message` says how.
- **Network.** `bytes_in`/`bytes_out` are HTTP body bytes counted by
  `rnk-registry`, not kaniko's wire bytes at the socket (`bytes_read`,
  `bytes_written`, with TLS and framing); no `conns`/`reused` and no
  `network.registry` block. `resources.*.netdev` is unchanged and remains
  the total.
- **Resources.** No `go_memstats`; `delta` is new. `summarize.py`'s
  resources table must skip the Go rows.
- **Exit.** A panic has its own status, `panic`, with code 134 in release
  builds (`SIGABRT`); kaniko reports it as `error` with code 2.
- **Meta.** No Go fields, `feature_flags` or `vcs_*`; `options` comes with
  the CLI.
- **Profiles.** No `KANIKO_PPROF_DIR` equivalent
  ([Q-057](../open-questions.md#q-057-does-ranikust-offer-cpu-and-heap-profiling)).

## Failure handling

| Way out | Report | Exit code |
|---|---|---|
| Success | written by `Telemetry::finish` | 0 |
| An error returned to `main` | written by `finish`, `exit.status = error` | 1, unchanged |
| A panic, release build (`panic = "abort"`) | written by the panic hook, after the panic message; open spans count as `open`, open records as `incomplete` | 134 (`SIGABRT`) |
| A panic, unwinding build (tests) | the hook writes a provisional report; a later `finish` replaces it and counts `exit.panics` | as the process exits |
| `SIGKILL`, OOM kill | none; the cgroup's `memory.events` `oom_kill` shows in the next run | |
| The report cannot be written | a warning on stderr | unchanged |

The panic hook never blocks on the span state: if another thread holds the
lock for 100 ms, or the panicking thread holds it, the report is written
without span sections and says `spans_unavailable`. One report is written
at a time: a panic report is dropped while another write is in progress
and never follows the normal exit's. The temporary file is created with
`O_EXCL` under a unique name, so nothing already there is truncated or
followed. Warnings go to stderr without `eprint!`, which would panic, and
abort, on a closed or full stderr.

## Determinism

The report is the only file metrics write, at exit, after the last snapshot,
export and push; in kaniko mode, even a path inside `/` is written after
every walk. No library reads metric state, and span fields are only
recorded, so layer and config bytes do not depend on metrics being on. The
check, as in kaniko: a `--reproducible` build gives the same digests with
`--metrics-file` given and not; `rnk-builder` adds it as a test with
its outputs (Phase 1 task 11).

## Testing hooks

- `rnk_metrics::Capture` runs code under a metrics-only subscriber and
  returns the span sections, for a crate's tests of its own spans. Put
  those tests in a test binary of their own (`rnk-layer`'s
  `tests/spans.rs`) and create the `Capture` before the first span is
  reached: tracing caches per process whether a span is wanted, and a test
  reaching it while no subscriber is alive can race a `Capture` into
  caching "never" (seen as a flaky missing span).
- `RANIKUST_DEBUG_FAULT=panic|error` (hidden) makes the binary panic or fail
  after metrics are set up; `crates/ranikust/tests/metrics.rs` uses it to
  check every exit path. It is honoured in release builds too, so the
  `panic = "abort"` path can be checked on the shipped binary.

## Cost

- Off: a `debug_span!` is one comparison against tracing's global level
  (`INFO`); no allocation, no lock.
- On: per span, a slot in the subscriber's registry, two short holds of the
  state lock and a few field writes, about a microsecond. Per report: a few
  `/proc` and cgroup reads and one file write.

## Open questions

- Profiling (CPU, heap) like `KANIKO_PPROF_DIR`:
  [Q-057](../open-questions.md#q-057-does-ranikust-offer-cpu-and-heap-profiling).
- Export formats besides the file (OTLP, Prometheus):
  [Q-058](../open-questions.md#q-058-which-metric-export-formats-come-after-the-json-report).

## References

- kaniko metrics: `kaniko@88b6d2b75 pkg/metrics/metrics.go`, `report.go`,
  `stats.go`, `cmd/executor/cmd/metrics.go`.
- tracing: <https://docs.rs/tracing/latest/tracing/>; tracing-subscriber
  layers and per-layer filters:
  <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/layer/>.
- cgroup v2 files: <https://docs.kernel.org/admin-guide/cgroup-v2.html>;
  `getrusage(2)`: <https://man7.org/linux/man-pages/man2/getrusage.2.html>.
