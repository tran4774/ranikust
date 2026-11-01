# Open questions

Questions the ADRs and design documents do not answer yet, waiting for the
maintainer. This is the single register: an `[open: Q-NNN]` marker anywhere
in the repository points here.

## How to use it

- **Raising.** Do not guess an undecided question. Add it under
  [Open](#open) as the next free `Q-NNN`, mark the place that needs the answer
  with `[open: Q-NNN]`, and continue with work that does not depend on it, or
  with the smallest reversible choice, recorded under "Interim" in the entry.
- **Answering.** The maintainer answers in the entry (or in conversation,
  then copied here). Move the entry to [Answered](#answered) with the date and
  the answer, record the decision where it belongs (a new ADR, a design doc,
  a crate card), and remove the `[open: Q-NNN]` markers.
- **Ordering.** Entries that block a Phase 1 task come first, in task order
  ([phase-1.md](agents/phase-1.md#task-order)).

Entry format:

```markdown
### Q-NNN: <short question>

- **Blocks:** <Phase 1 task N / nothing yet>; **Area:** <crate or topic>
- **Context:** one or two lines, with links to where it was raised.
- **Options:** (a) …; (b) …
- **Recommendation:** … (or none)
- **Interim:** the reversible choice made meanwhile, if any.
- **Raised:** YYYY-MM-DD
```

## Open

<!-- Entries are added below this line. -->

### Q-015: Which digest names an extracted layer, and where do locks live?

- **Blocks:** Phase 1 task 7; **Area:** `rnk-store`
- **Context:** `EntryKey::Layer(Digest)` does not say which digest names
  `layers/<id>`, and the lock files' place in the state directory is open
  ([rnk-store](agents/crates/rnk-store.md#phase-1),
  [cache-and-store](design/cache-and-store.md#locks)).
- **Options:** layers named by (a) the compressed blob digest or (b) the
  diff ID; lock files (c) in one directory or (d) beside each entry.
- **Recommendation:** none
- **Interim:** (b) and (c) on `feat/rnk-store`: `layers/<algorithm>/<hex>`
  by diff ID, so a gzip and a zstd blob of one layer share one extraction,
  and the image config lists diff IDs; lock files in
  `locks/{blobs,layers}/<algorithm>/<hex>`, never removed, so a waiter never
  locks an unlinked file. Reversible: only `EntryKey`'s paths change, and
  nothing has published layers yet.
- **Raised:** 2026-10-30

### Q-016: How is the store lock protocol proven under crashes and races?

- **Blocks:** Phase 1 task 7; Phase 6; **Area:** `rnk-store`
- **Context:** races between extraction and deletion, crashes during an
  extraction and concurrent builds on one store are untested
  ([cache-and-store](design/cache-and-store.md#open-questions);
  [ADR-0006](adr/0006-persistent-store.md#more-information),
  [ADR-0007](adr/0007-run-anywhere-shared-store.md#more-information),
  [ADR-0021](adr/0021-rnk-store-crate.md#more-information)).
- **Options:** (a) the T0 multi-process and T2 `SIGKILL` tests planned on
  the [card](agents/crates/rnk-store.md#tests), in task 7; (b) the race
  cases left to the Phase 6 host-safety suite.
- **Recommendation:** (a): a lock race or a crash mid-extraction can
  publish a partial layer that later builds trust; proven when the store
  is built, not left for Phase 6 (2026-10-31).
- **Interim:** decided in Phase 6 (maintainer, 2026-10-31): the T0
  multi-process and T2 `SIGKILL` tests come with the Phase 6 host-safety
  suite.
- **Raised:** 2026-10-30

### Q-017: Which type carries the probe outcome to the libraries?

- **Blocks:** Phase 1 task 8; **Area:** `ranikust`
- **Context:** `ranikust` picks an `Outcome` from the crates' probe reports;
  the type the libraries receive is open
  ([ranikust](agents/crates/ranikust.md#api-planned),
  [modes-and-detection](design/modes-and-detection.md#open-questions)).
- **Options:** (a) one shared outcome type in a crate the libraries already
  use; (b) each library receives only its own settings (snapshot kind,
  sandbox variant).
- **Recommendation:** none
- **Interim:** (b) for the sandbox: `rnk_sandbox::probe` returns its own
  `Caps` (the root entry), and `Sandbox::start` takes `Option<Caps>`,
  `None` allowing only `Isolation::Direct` (`feat/rnk-sandbox`).
  Reversible: `ranikust` is the only caller. `feat/ranikust-setup` keeps
  (b): `ranikust`'s own `Outcome` (in `src/setup.rs`) holds each crate's
  caps; since `feat/rnk-snapshot` the overlay part is
  `rnk_snapshot::OverlayCaps` (the mount API), which
  `rnk_snapshot::Overlay::new` takes.
- **Raised:** 2026-10-30

### Q-018: Where does the probe mount its test overlay?

- **Blocks:** Phase 1 tasks 8, 9; **Area:** `ranikust`, `rnk-snapshot`
- **Context:** with `--mode=auto` and no `--state-dir`, the state directory
  depends on the outcome, yet probe steps 1, 2 and 4 mount a test overlay
  where the stage roots will live
  ([modes-and-detection](design/modes-and-detection.md#probe-order---modeauto),
  [cache-and-store](design/cache-and-store.md#open-questions),
  [ADR-0022](adr/0022-common-state-dir.md#more-information)).
- **Options:** (a) under the state directory (`/var/lib/ranikust` by
  default since ADR-0050); (b) in a probe-only temporary directory.
- **Recommendation:** none
- **Interim:** (a), because Phase 1 has no outcome but overlay mode:
  `feat/ranikust-setup` probes in the state directory (`--state-dir`,
  default `/var/lib/ranikust`, ADR-0050); the sandbox probe uses its
  `sandbox/` subdirectory, and the overlay probe (`feat/rnk-snapshot`)
  this build's `builds/<id>/overlay-probe`, deleted with the build.
  Revisit with kaniko mode (Phase 2).
- **Raised:** 2026-10-30

### Q-152: How are read-only layer roots published without root?

- **Blocks:** Phase 3 (rootless overlay); **Area:** `rnk-store`,
  `rnk-snapshot`
- **Context:** publishing an extracted layer renames its directory from
  `tmp/` into `layers/`; renaming a directory to another parent needs write
  permission on it, so a layer whose root is `0555` (some images) fails
  with `EACCES` without `CAP_DAC_OVERRIDE`. As root (Phase 1) it works.
- **Options:** (a) give the layer root `u+w` until it is published, then
  set its mode; (b) extract into the final directory under the lock and
  publish a marker instead of renaming.
- **Recommendation:** none
- **Interim:** none; Phase 1 runs as root.
- **Raised:** 2026-10-30

### Q-027: Is the ContextSource trait sync or async?

- **Blocks:** Phase 1 task 10; **Area:** `rnk-context`
- **Context:** the trait is defined in Phase 1 for local directories, but
  its HTTPS and git sources (Phase 5) do network I/O
  ([rnk-context](agents/crates/rnk-context.md#api),
  [Phase 1](agents/crates/rnk-context.md#phase-1)).
- **Options:** (a) sync; (b) async.
- **Recommendation:** none
- **Interim:** sync, on branch `feat/rnk-context`: `ContextSource::open`
  returns `Result<Context>`, and the only source is a local directory.
  Reversible: the trait has one implementation and no caller outside the
  crate; making it async changes `open`'s signature and its callers in
  `rnk-builder`, nothing else.
- **Raised:** 2026-10-30

### Q-028: Which planned crate dependencies are allowed?

- **Blocks:** Phase 1 tasks 10, 12; Phases 4, 5; **Area:** architecture
- **Context:** undecided edges ([overview](agents/overview.md#crate-graph)
  and the crate cards' "Used by" lines): `rnk-context` → `rnk-ignore`
  (task 10); `rnk-sched` → `rnk-dockerfile`, the stage graph type (task 12);
  `rnk-cache` → `rnk-registry`, `rnk-dockerfile`, `rnk-ignore`, `rnk-layer`
  (Phase 4); `rnk-context` → `rnk-layer` for tar contexts, and to the TLS
  configuration that `rnk-registry` owns since `ADR-0024` (Phase 5);
  `ranikust` → `rnk-registry` for `push` (Q-048; `login` is dropped, ADR-0049).
- **Options:** per edge, (a) allow the direct dependency; (b) route it
  through `rnk-builder`, which already depends on all of them.
- **Recommendation:** none
- **Interim:** `rnk-context` → `rnk-ignore` is declared on branch
  `feat/rnk-context`: ADR-0027 makes the context walk call
  `matches_using_parent` and `may_reinclude_below`, which only work inside
  the walk, so routing through `rnk-builder` would move the walk there.
  Reversible: one dependency line, and `rnk-ignore` is pure data.
  `rnk-sched` → `rnk-dockerfile` is declared on branch `feat/rnk-sched`:
  the scheduler walks `StageGraph::needed` and `StageNode::dependencies()`.
  Reversible: the graph type is pure data, and routing it through
  `rnk-builder` would only move the import. The other edges are still
  undeclared.
- **Raised:** 2026-10-30

### Q-029: Which crate computes LayerDemand?

- **Blocks:** Phase 1 tasks 11, 12; **Area:** `rnk-builder`, `rnk-sched`
- **Context:** `LayerDemand` belongs to `rnk-builder` and travels in
  `StageJob`; which crate derives it from the graph and the outputs is open
  ([rnk-builder](agents/crates/rnk-builder.md#phase-1),
  [rnk-sched](agents/crates/rnk-sched.md#api-planned)).
- **Options:** (a) `rnk-sched`, which holds the graph; (b) `rnk-builder`.
- **Recommendation:** none
- **Interim:** both halves, on branch `feat/rnk-builder-outputs`:
  `rnk-builder` owns the rule, `LayerDemand::for_stage(in_target, outputs,
  cache)`, and `rnk-sched` (task 12) tells it whether a stage is the target
  or an ancestor of it through `FROM <stage>`. Reversible: moving the call
  moves one line.
- **Raised:** 2026-10-30

### Q-030: What are the memory-limit algorithm and thresholds?

- **Blocks:** Phase 1 task 12; **Area:** `rnk-sched`
- **Context:** `ADR-0013` decides a memory-aware limit; the design reads
  `memory.current` against `memory.max`, else `MemAvailable`, and counts
  page cache in kaniko mode, but the algorithm is not designed
  ([scheduler](design/scheduler.md#limits),
  [rnk-sched](agents/crates/rnk-sched.md#api-planned),
  [phase-1.md](agents/phase-1.md#open-questions-blocking-phase-1)). W3 has
  three stages, so more stages than cores is unmeasured.
- **Options:** none listed; thresholds come from measurement.
- **Recommendation:** none
- **Interim:** add the hook and measure in Phase 1; thresholds stay open
  ([rnk-sched](agents/crates/rnk-sched.md#phase-1)). On branch
  `feat/rnk-sched` the hook is the `MemoryGate` trait, checked before each
  stage starts; `SystemMemory` admits a stage while free memory is at
  least an eighth of the limit and at least 512 MiB, and always when no
  stage runs. Free memory under a cgroup v2 limit is `memory.max` less the
  working set, `memory.current` minus `inactive_file` from `memory.stat`
  (the kubelet's measure): page cache from pulls and extraction would
  otherwise hold every later stage back. Without a limit it is
  `MemAvailable`. The gate admits at most one stage per fresh reading,
  500 ms or more after its last admission, so stages that become ready
  together (W3's peak) do not all start on one reading. Reversible: one
  function and two constants, and the gate is replaceable.
- **Raised:** 2026-10-30

### Q-032: What remains to verify in the RUN sandbox?

- **Blocks:** Phase 1 task 13; Phases 3, 6; **Area:** `rnk-sandbox`
- **Context:** `--seccomp`, `--userns` and `--network=none` have not run end
  to end; symlink races (TOCTOU) on bind targets and the host-safety attack
  suite are Phase 6 work ([sandbox](design/sandbox.md#open-questions);
  [ADR-0003](adr/0003-dockerfile-trust-and-default-isolation.md#more-information),
  [ADR-0010](adr/0010-run-network-host-or-none.md#more-information)).
  Phase 1 acceptance needs `--network=none`
  ([phase-1.md](agents/phase-1.md#acceptance-criteria)).
- **Options:** none listed; each item needs a test in its phase.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-033: Which performance conditions are still unmeasured?

- **Blocks:** Phase 1 task 13; Phase 2; **Area:** performance
- **Context:** not measured: sha256 of large layers with `sha2` against
  `ring::digest`, which the TLS provider already brings in (Q-012 kept
  `sha2`; the switch stays inside `rnk_layer::Sha256Digest`); real network
  registries, where overlapping
  pulls may matter more than CPU
  ([scheduler](design/scheduler.md#open-questions));
  export on memory-starved runners, where one layer took 1.8–6.6 s
  ([overlay-snapshot](design/overlay-snapshot.md#open-questions)); cold page
  cache, slow disks and deletion of very large roots in kaniko mode
  ([kaniko-mode](design/kaniko-mode.md#open-questions)); `--snapshot-mode=redo`
  and `time` against `full`, which decides whether they stay (ADR-0023,
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md)).
- **Options:** none listed.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-039: Which environments must the probe be tested in?

- **Blocks:** Phase 3; **Area:** `ranikust`, `rnk-sandbox`, `rnk-snapshot`
- **Context:** untested: Kubernetes (containerd's `cri-containerd` AppArmor
  profile, `procMount: Unmasked`, `hostUsers: false`, Pod Security
  Standards); kernels other than 7.0 and 5.15 (5.10, 6.1, RHEL 8/9); SELinux
  with rootless overlay, Docker rootless, fuse-overlayfs, real CI runners
  ([modes-and-detection](design/modes-and-detection.md#open-questions),
  [ADR-0004](adr/0004-support-every-privilege-level.md#more-information)).
- **Options:** none listed.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-040: What happens to a rootless user without subuid ranges?

- **Blocks:** Phase 3; **Area:** `rnk-sandbox`, `ranikust`
- **Context:** without `newuidmap` and `/etc/subuid` only one uid maps, and
  `chown` to any other uid fails with `EINVAL`
  ([modes-and-detection](design/modes-and-detection.md#probe-order---modeauto),
  [open questions](design/modes-and-detection.md#open-questions)).
- **Options:** (a) refuse images with several owners; (b) an
  `ignore_chown_errors`-style flag, as podman offers.
- **Recommendation:** (a): (b) silently gives files the wrong owner, so a
  file meant for a service user becomes the build user's, a change of
  permissions nobody reviewed; podman's flag is not a reason to copy it
  (2026-10-31).
- **Interim:** decided when Phase 3 starts (maintainer, 2026-10-31).
- **Raised:** 2026-10-30

### Q-041: What exactly goes into a cache key and its tag?

- **Blocks:** Phase 4; **Area:** `rnk-cache`, `rnk-builder`
- **Context:** `key(k) = H(version, key(k-1), instruction, inputs(k))`; the
  chain root, the instruction form and the inputs are `[inferred]` and fixed
  in Phase 4 with W2 ([cache-and-store](design/cache-and-store.md#cache-keys));
  `KeyInputs`, `Executor::cache_inputs`, `H` and the tag in `--cache-repo`
  are open ([rnk-cache](agents/crates/rnk-cache.md#api-planned),
  [rnk-builder](agents/crates/rnk-builder.md#api-planned)).
- **Options:** `H`: (a) SHA-256 or (b) another cryptographic hash; fields
  and tag: designed with W2.
- **Recommendation:** SHA-256 for `H`, "likely" on the
  [rnk-cache](agents/crates/rnk-cache.md#api-planned) card; none for the rest.
- **Note (2026-11-01):** in kaniko mode on `/` a `RUN`'s cache mount is
  left out (ADR-0057), so what the `RUN` writes at the cache target lands
  in its layer, which it never does with the mount. The key must tell the
  two apart: the mode (or "cache mounts dropped") is an input of a `RUN`
  with cache mounts, so a layer built without the mount is never reused
  where the mount is given, nor the other way round. `rnk-cache` has no
  keys yet (Phase 4).
- **Interim:** none
- **Raised:** 2026-10-30

### Q-042: How do cache lookahead and cached-stage skipping work?

- **Blocks:** Phase 4; **Area:** `rnk-cache`, `rnk-sched`, `rnk-builder`
- **Context:** Phase 4 adds cache lookahead and skips fully cached stages;
  no design covers them yet
  ([cache-and-store](design/cache-and-store.md#open-questions),
  [rnk-cache](agents/crates/rnk-cache.md#phase-1),
  [rnk-sched](agents/crates/rnk-sched.md#phase-1)). Lazy snapshots depend
  on `--cache` too (Q-034).
- **Options:** none listed.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-250: Does a git context fall back between the two backends?

- **Blocks:** Phase 5; **Area:** `rnk-context`
- **Context:** `ADR-0035` clones with a built-in `gix` backend by default
  and an external `git` on request (`--git-backend`). The built-in may not
  support a protocol or option the external `git` does.
- **Options:** (a) no fallback: the built-in fails with an error naming
  `--git-backend=external`; (b) fall back to an external `git` found in
  `PATH`, with a warning.
- **Recommendation:** (a): a fallback would read the host's `git` and
  configuration unasked.
- **Interim:** none
- **Raised:** 2026-10-30

### Q-251: Can kaniko's `#ref#commit` fragment name a subdirectory?

- **Blocks:** Phase 5; **Area:** `rnk-context`
- **Context:** kaniko's fragment is `#ref#commit` (`kaniko@44aa7ddc0
  pkg/buildcontext/git.go:58`, `:126-128`) and has no subdirectory;
  BuildKit's is `#ref:subdir` (`buildkit@cd8ee244
  util/gitutil/git_url.go:71-79`). `ADR-0035` accepts both.
- **Options:** (a) refuse a `:subdir` with kaniko's form; (b) accept
  `#ref:subdir#commit`; (c) use `--context-sub-path` only; (d) drop
  kaniko's `#ref#commit` form and accept BuildKit's `#ref:subdir` only,
  pinning by a full commit hash as the ref.
- **Recommendation:** (d): one fragment grammar, so no URL is read two
  ways; a full commit hash pins the source as kaniko's second part did.
  Accepting kaniko's form only served compatibility (2026-10-31).
- **Interim:** decided when Phase 5 starts, with ADR-0035 (maintainer,
  2026-10-31).
- **Raised:** 2026-10-30

### Q-252: Which `--git-config` keys does the built-in backend honour?

- **Blocks:** Phase 5; **Area:** `rnk-context`
- **Context:** `ADR-0035` passes `--git-config` to the external `git` as
  `GIT_CONFIG_*` and to `gix` as configuration overrides; `gix` reads only
  part of git's keys.
- **Options:** (a) a documented allow-list (proxy, CA file, extra headers,
  `url.*.insteadOf`, `core.sshCommand`), others refused; (b) pass every key
  and let `gix` ignore the rest.
- **Recommendation:** (a), so a key that silently does nothing cannot
  surprise.
- **Interim:** none
- **Raised:** 2026-10-30

### Q-245: Should one overlay mount serve consecutive instructions?

- **Blocks:** nothing yet; **Area:** `rnk-snapshot`
- **Context:** each instruction mounts and unmounts its own overlay; one
  mount for several consecutive instructions would save that cost, against
  the `volatile` remount rules
  ([overlay-snapshot](design/overlay-snapshot.md#the-volatile-lifecycle)).
  BuildKit mounts per exec, so it has no counterpart. Split out of Q-049.
- **Options:** (a) build it after Phase 1; (b) drop it.
- **Recommendation:** (a), behind a measurement: time the mount and
  unmount per instruction on W1–W3; build it if it is a visible share of
  a build, with the `volatile` rules unchanged (2026-10-31).
- **Interim:** measure first (ADR-0052): time the mount and unmount per
  instruction on W1–W3; nothing is built before that.
- **Raised:** 2026-10-30

### Q-057: Does ranikust offer CPU and heap profiling?

- **Blocks:** nothing yet; **Area:** `rnk-metrics`
- **Context:** kaniko's fork writes `cpu.pprof` and `heap.pprof` when
  `KANIKO_PPROF_DIR` is set (`kaniko@88b6d2b75 pkg/metrics/metrics.go:82-109`);
  `ADR-0025` leaves profiling out of the metrics report
  ([metrics](design/metrics.md#comparing-with-kaniko)). Spans show which
  phase is slow, not which function.
- **Options:** (a) none: `perf record` on the static binary (symbols are
  kept, `strip = "debuginfo"`); (b) an opt-in sampling profiler writing
  pprof (`pprof` crate, signal-based; check musl and aarch64); (c) heap
  statistics from mimalloc in the report.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-058: Which metric export formats come after the JSON report?

- **Blocks:** nothing yet; **Area:** `rnk-metrics`
- **Context:** `ADR-0025` writes one JSON file per build. Survey 03 named
  `opentelemetry-otlp` ([refs](agents/refs.md#crates)); kaniko exports
  OpenTelemetry traces. A CI service may want spans in a tracing backend or
  counters scraped by Prometheus
  ([metrics](design/metrics.md#open-questions)).
- **Options:** (a) the file only; (b) an OTLP exporter layer behind a flag
  or `OTEL_*` variables, which starts a thread and a runtime (after
  set-up only); (c) a Prometheus text file written at exit
  (node-exporter textfile collector); (d) a Prometheus endpoint, which
  needs a long-lived process.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-360: What does kaniko mode on `/` do after an earlier build in the container was cut short?

- **Blocks:** nothing yet; **Area:** `rnk-store`, `ranikust`
- **Context:** found by the host-safety review of ADR-0056: the fence
  keeps names away from this build's `RUN` processes only. When an earlier
  ranikust in the same container was killed mid-`RUN` and was not PID 1,
  its `RUN`'s processes live on, outside any fence and any sweep, and
  could open a name in the moment it exists. ranikust sees the trace: its
  marker `.ranikust-building` left in the state directory, or leftover
  `.ranikust-anon-*` names.
  The marker holds the PID namespace and boot id: one from another
  container or boot is stale and taken over. It covers accidental kills
  only: a `RUN` can delete the marker and then try to get ranikust killed;
  leftover names with no marker still count. A `RUN` that failed after
  its command ran, or was cancelled, also leaves the marker (its sweep may
  not have finished), so later builds in the container use memory too.
- **Options:** (a) make no name for that build: state in `memfd`, with a
  warning saying why and how to avoid it (run ranikust as PID 1, or
  restart the container), the marker kept for later builds; a `COPY
  --from` a kept stage or an image on `/`, which needs a named directory,
  fails the build with that advice; (b) refuse the build; (c) kill every
  process that is not an ancestor of ranikust, which cannot be done
  safely: the container's other processes (a CI shell, a sidecar) are not
  identifiable as the dead build's; (d) make names only when ranikust is
  PID 1 (no process of a dead build can outlive it there), `memfd`
  otherwise: safe against a deleted marker too, at a memory cost under a
  CI shell, the common case.
- **Recommendation:** (a). The build still succeeds unless it needs a
  `COPY --from`, and it costs only memory, in a container where something
  unusual already happened. Choose (d) instead if a `RUN` deleting the
  marker must be covered too.
- **Note:** an anonymous store holds an exclusive `flock` on its state
  directory, so two containers sharing a volume never build on it at the
  same time (the second is refused): a marker is taken over only from a
  dead build.
- **Interim:** (a), on `feat/kaniko-state-on-disk` (`rnk_store::Store::open_anonymous`).
- **Raised:** 2026-11-01

### Q-370: Should a built layer be encoded straight into its upload?

- **Blocks:** nothing; **Area:** `rnk-builder`, `rnk-registry`
- **Context:** ADR-0055 pushes a built layer as soon as it is exported,
  from a file that lives one upload long. `oci-client`'s
  `push_blob_stream_chunked` could take the encoder's output directly and
  compute the digest while sending, saving that file
  ([streaming](design/streaming.md#built-layers-pushed-as-soon-as-they-are-exported)).
- **Options:** (a) keep the one-layer file: the existence check before
  sending, retries of a failed request, one encode; (b) encode into the
  request: no file, but every layer sent even when the registry has it, a
  failed request re-encodes the layer from the root, chunked uploads only.
- **Recommendation:** (a) until a measurement shows the one-layer file
  matters; (b) costs bandwidth and CPU on every rebuild.
- **Interim:** (a).
- **Raised:** 2026-11-01

### Q-383: How do `bind`, `tmpfs` and `from=` mounts work for a `Direct` `RUN` on `/`?

- **Blocks:** Phase 5 (those mounts); **Area:** `rnk-builder`
- **Context:** `RUN --mount=type=bind|tmpfs|ssh` and `from=` are refused
  in every mode until Phase 5. In overlay mode and the full sandbox they
  will be binds; on `/` (ADR-0057) nothing can be bound, and a bind from
  a kept stage (`from=<stage>`) would need its tar extracted somewhere
  the `RUN` reads.
- **Options:** (a) refuse them on `/`, as Q-380 and Q-381; (b) copy the
  source in, read-only by convention only, removed before the walk; (c)
  `tmpfs` alone as an empty directory removed before the walk.
- **Recommendation:** decide with Phase 5; (a) unless the copy cost and
  the crash residue of (b) are acceptable for read-only sources.
- **Interim:** refused in every mode (Phase 5).
- **Raised:** 2026-11-01

## Answered

### Q-410: How do per-stage directories stop costing more than kaniko on W1?

- **Answer (2026-11-01):** "1. ok" (to the recommendation, option (c));
  then "ok làm Q410c trước". The last stage to read a parent takes its
  directory over instead of copying it (pruned of what the copy leaves
  out, then moved with `renameat2(RENAME_NOREPLACE)`), and a stage
  directory nobody reads any more is released at once on a release
  thread, the build's end waiting for it. Recorded in
  [ADR-0061](adr/0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md);
  which reader takes the directory is decided from the stage graph (its
  only reader), never by scheduling:
  [ADR-0062](adr/0062-stage-directory-hand-over-decided-from-the-stage-graph.md).
- **Context (as asked):** the first VM `e2e` run with kaniko mode took
  31.3 s on W1 as root on the VM with a directory per stage, against
  kaniko's 23.7 s: `FROM <stage>` copies 7.2 s (ext4, no reflink), the
  release at the end 4.2 s.

### Q-400: How does `--parallel-stages` run stages at once on a container's `/` without mounts?

- **Answer (2026-11-01):** "Ok làm theo đề xuất của bạn": option (a).
  `--parallel-stages` stays refused with exit 2 and guidance, whatever
  the environment: outside a container stages already run in parallel
  with the full sandbox (ADR-0059), and on a container's `/` a bare
  `chroot` per stage lets a `RUN` escape to what ADR-0051 and ADR-0056
  keep out of its reach. Option (b), a hardened bare `chroot`
  (capabilities dropped after the `chroot`, static fake `/proc`, no
  procfs), may be reconsidered only after `poc-runner` measures its escape
  vectors in the VM (rootful Docker, Kubernetes' default profile,
  `ptrace_scope` 0 and 1); option (c), ADR-0014 as written, never.
  Recorded in
  [ADR-0060](adr/0060-copy-leaves-special-files-out-where-run-has-no-nodev.md).

### Q-380: How does a `Direct` `RUN` on `/` get a secret as a file?

- **Answer (2026-11-01):** "Q380-382 làm theo đề xuất": option (a). A
  secret as a file (no `env=`, or a `target=`) is refused in kaniko mode
  on `/`, pointing to `env=` or overlay mode; a secret never touches the
  image's filesystem. Options (b) a file on `/` removed before the walk,
  (c) a symlink to ranikust's memfd and (d) an inherited memfd with its
  `/dev/fd` path were not taken. Recorded in [ADR-0057](adr/0057-direct-run-heredoc-from-memfd-and-refusals.md).

### Q-381: How does a `Direct` `RUN` on `/` get a cache mount?

- **Answer (2026-11-01):** "Q380-382 làm theo đề xuất": option (c). A
  cache mount of a `RUN` in kaniko mode on `/` is left out with a warning
  and the `RUN` runs without the cache; the warning says that a
  `sharing=locked` cache then serializes nothing. A cache with `from=`
  stays refused (Phase 5). Option (b), copying the cache in and out, was
  not taken. Recorded in [ADR-0057](adr/0057-direct-run-heredoc-from-memfd-and-refusals.md).

### Q-382: What does `--network=none` do for a `Direct` `RUN` on `/`?

- **Answer (2026-11-01):** "Q380-382 làm theo đề xuất": option (a). The
  `--network=none` flag is refused at start-up, before the pull, and a
  `RUN --network=none` at that `RUN`; a build that asked for no network
  never gets the container's. Options (b) a seccomp filter and (c) a user
  namespace remain possible later; (d) is never taken. Recorded in [ADR-0057](adr/0057-direct-run-heredoc-from-memfd-and-refusals.md).

### Q-385: Does a step that changes nothing still add an empty layer?

- **Answer (2026-11-01):** "Q385 làm như kaniko": option (b). A step
  whose layer is empty adds no layer, only its history entry with
  `empty_layer: true`, in both modes; this changes Q-131's answer for
  such steps. Raised from the issue mining (moby/buildkit#6543: repeated
  empty layers refused by containerd). Recorded in
  [ADR-0058](adr/0058-a-step-that-changes-nothing-adds-no-layer.md).

### Q-011: Who owns the workdir checks, and what counts as empty?

- **Answer (2026-10-30):** options (b) and (d): `ranikust` runs the workdir
  checks during process set-up and holds the kaniko-mode workdir `flock` for
  the build; `rnk-rootfs` provides the primitives only. A workdir of only
  mount points is refused. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-019: What is the final name of the parallel-stages flag?

- **Answer (2026-10-30):** option (a): `--parallel-stages` is the final
  name. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-034: When must a lazily skipped stage still be snapshotted?

- **Answer (2026-10-30):** the maintainer took the recommendation: decided
  once from the stage graph before the build. A stage's layers are needed
  when it is the target or a stage whose layers are needed builds `FROM` it;
  any other stage is not walked. Nothing is decided later, so no deletion
  has to be found after the fact; `--cache` (Phase 4) only adds inputs to
  the rule (Q-042). Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-035: What does the kaniko-mode ignore list hold?

- **Answer (2026-10-30):** options (a) and (c): all of `/ranikust`, and
  kaniko's `/etc/mtab`, `/tmp/apt-key-gpghome` and `/var/run` (while
  `--ignore-var-run` is true), besides mount points, `--ignore-path`,
  `/proc`, `/sys`, `/dev` and the state directory. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-036: What is the state directory default for kaniko mode on a host?

- **Answer (2026-10-30):** a third option from the maintainer: with the root
  `/` the default stays `/ranikust/state`; with any other root it is
  `<cwd>/.ranikust`, a hidden directory in the directory ranikust was
  started in, ignored whenever it lies inside the root. Recorded in
  [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-037: What else must the fake proc cover?

- **Answer (2026-10-30):** option (b): keep the fourteen files until a build
  fails without `memory.stat`; the untried tools get tests in Phase 2 task
  10. Recorded in [ADR-0038](adr/0038-kaniko-mode-settings-for-phase-2.md).

### Q-038: Which crates fill the Phase 2 dependency picks?

- **Answer (2026-10-30):** options (b) and (d), after the benchmark of
  Phase 2 task 0 (`tests/bench/walk-hash`): the walk is a `rayon` scope over
  `openat2` and `getdents64`, the content hash xxh3-128 from `xxhash-rust`.
  The walk runs on 8 workers by default; the maintainer asked for a flag to
  change it, `--snapshot-workers`. Recorded in
  [ADR-0039](adr/0039-kaniko-mode-walk-and-hash-crates.md).

### Q-270: Are device nodes from an image created in a host-workdir root?

- **Answer (2026-10-31):** option (b), by the rule to follow kaniko:
  `extract_live` creates no block or character device node and no FIFO in a
  live root, `/` and host workdirs alike, as kaniko's `ExtractFile`
  (kaniko@44aa7ddc0 `pkg/util/fs_util.go:440-575`); they are counted as
  `nodes_skipped`, and the image keeps them through its unchanged base
  layers. Recorded in
  [ADR-0041](adr/0041-live-root-extraction-skips-device-nodes.md).

### Q-300: Is a stage whose image is kept for a later `FROM` walked in sequential kaniko mode?

- **Answer (2026-10-31):** option (a), kaniko's `SaveStage` behaviour:
  every unit with `save_image` is walked. Recorded in
  [ADR-0044](adr/0044-kaniko-mode-walks-every-kept-stage.md).

### Q-024: What is the Snapshotter signature?

- **Answer (2026-10-31):** the interim shape, with associated types:
  `Snapshotter::Layer` (a base layer as the mode keeps it) and
  `StageRootfs::Diff: ExportLayer` (a closed layer), and
  `StageRootfs::set_layers_needed` for lazy snapshots, so overlay and kaniko
  mode share one builder. Recorded in
  [ADR-0046](adr/0046-snapshotter-signature.md).

### Q-320: What keeps a kaniko-mode host workdir out of other users' reach?

- **Answer (2026-10-31):** option (a), chosen by the maintainer: a real
  parent of the workdir that group and others cannot search, and it and
  everything below it down to the workdir owned by root and not writable
  by others; directories above it may belong to anyone; a missing
  `nosuid,nodev` is warned about. Recorded in
  [ADR-0047](adr/0047-kaniko-workdir-chain-is-private.md).

### Q-321: Is the container check enough to guard a kaniko-mode build on `/`?

- **Answer (2026-10-31):** option (a), by the maintainer: kaniko's
  container heuristic and `--force`, so a container build needs no extra
  flag; revisit with (b) if a host is seen to match the heuristic.
  Recorded in [ADR-0048](adr/0048-container-heuristic-guards-the-root.md).

### Q-008: What does a repeated target flag mean?

- **Answer (2026-10-31):** option (b): one `--target`; a repeated flag or a
  comma-separated list is a usage error (exit 2). The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-010: Which path type does rnk-ignore match on?

- **Answer (2026-10-31):** option (b), bytes, as built: a file whose name is not
  UTF-8 still matches an exclude. The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-047: What are the mirror, registry-map and helper flags called?

- **Answer (2026-10-31):** option (c): a mirror or mapped registry never
  receives another registry's credentials and never serves a push; the
  flags are named when Phase 5 builds them, kaniko's names only where the
  meaning is exactly that. The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-260: Should overlay mode honour kaniko's snapshot flags?

- **Answer (2026-10-31):** option (d): overlay mode refuses `--ignore-path`,
  `--single-snapshot` and `--ignore-var-run=false` for good; kaniko mode
  keeps the first two while a case needs them and `--single-snapshot` only
  if measured faster. The maintainer, with the priority of
  security, then performance; recorded in
  [ADR-0049](adr/0049-security-and-performance-over-kaniko-compatibility.md).

### Q-048: What does the push subcommand take, and when does it come?

- **Answer (2026-10-31):** an OCI layout only, what `--oci-layout-path`
  writes; no docker-save tar; no phase yet. `login` is dropped (ADR-0049).
  Recorded in [ADR-0052](adr/0052-push-input-aliases-and-issue-mining-fixes.md).

### Q-110: What is the overlay-mode state directory without `--workdir`?

- **Answer (2026-10-31):** a fixed default, `/var/lib/ranikust` as root in
  every mode, `--state-dir` to change it; `--workdir` is gone: ranikust
  picks the build root itself. Recorded in
  [ADR-0050](adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md).

### Q-162: Where do the blobs of built layers live?

- **Answer (2026-10-31):** option (b), in Phase 4: published in the
  persistent store under the digest computed while writing, for overlay
  mode and kaniko mode on a host; kaniko mode on `/` keeps no state
  between runs (ADR-0051). Recorded in
  [ADR-0052](adr/0052-push-input-aliases-and-issue-mining-fixes.md).

### Q-330: What does kaniko mode do on a host where root may not mount?

- **Answer (2026-10-31):** option (a), by the maintainer: refused, with the
  advice to run ranikust in a container or give it mount privileges; `RUN`
  would otherwise run in a bare `chroot` on the host (the options were (a)
  refuse, (b) an opt-in flag, (c) a warning). Recorded in
  [ADR-0050](adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md).

### Q-340: Where do kaniko mode's anonymous files live when the container's root has no O_TMPFILE?

- **Answer (2026-11-01):** the maintainer asked for ranikust's memory to be
  as small as possible, safely ("Làm 1 và 2 đi. Làm sao tối ưu bộ nhớ nhất
  và an toàn là được"): on disk. Where `O_TMPFILE` is refused, a file made
  by a random name and unlinked at once, only while no `RUN` process
  exists (a fence in the store); `memfd` is the last fallback, with a
  warning. Streaming layers (option 1) is ADR-0055. Recorded in
  [ADR-0056](adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md).

### Q-350: How does kaniko mode on `/` keep a stage's files out of `RUN`'s reach?

- **Answer (2026-11-01):** option (a), as ADR-0051 decided for all state on
  `/`: a kept stage is a plain tar in an anonymous file, an image read by
  `COPY --from` stays its blobs, and a reader's view is unpacked in the
  state directory only while no `RUN` runs and deleted at the end of the
  instruction that read it. Recorded in
  [ADR-0056](adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md).

### Q-140: How is the aarch64 binary made static-pie with symbols?

- **Answer (2026-10-30):** the maintainer chose a fourth option: link with
  a gcc driver that targets aarch64, as x86_64 links with the host `cc`:
  the native `cc` on an aarch64 machine (CI's `ubuntu-24.04-arm` runner)
  and `aarch64-linux-gnu-gcc` (package `gcc-aarch64-linux-gnu`) on
  x86_64. The linker alone is not enough: rustc 1.99's
  `aarch64-unknown-linux-musl` target has no static-pie and links
  `-static -no-pie` with `crt1.o` (forcing `-static-pie` over it gives a
  binary that crashes, since `crt1.o` does not relocate itself). `just
  test-aarch64` therefore builds PIE code (`-C relocation-model=pie`),
  turns off rustc's own start files (`-C link-self-contained=no`) and links
  musl's self-relocating `rcrt1.o`, `crti.o`, `crtbeginS.o`, `libc.a`,
  `crtendS.o` and `crtn.o` from rustc's self-contained directory with
  `-static-pie -nostdlib`, as rustc does by itself for x86_64. Measured on
  x86_64 with `aarch64-linux-gnu-gcc` 15.2 and QEMU 10.2: `static-pie
  linked, not stripped`, no interpreter, no `NEEDED`, no glibc symbol;
  every T0 test passes. Revisit when rustc gives the target static-pie
  (the flags can then go). Without a cross gcc the recipe falls back to
  zig's linker and warns. See
  [development-setup](development-setup.md#2-aarch64-when-you-touch-anything-platform-specific).

### Q-020: Is custom-platform accepted as an alias of platform?

- **Answer (2026-10-30):** the maintainer chose option (a):
  `--custom-platform` is an alias of `--platform`, and the deprecated
  `--customPlatform` too, with kaniko's warning. Recorded in
  [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md).

### Q-021: What do the cache flags do before Phase 4?

- **Answer (2026-10-30):** option (a) with one exception: `--cache`,
  `--cache=true` and `--cache-repo` are refused naming Phase 4, and
  `--cache=false`, which asks for what Phase 1 does, is accepted. Recorded
  in [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md).

### Q-023: Which other kaniko flags and variables are accepted?

- **Answer (2026-10-30):** the maintainer approved the per-flag list:
  accepted with kaniko's meaning, accepted and ignored with a warning where
  the image is the same without the flag (extending `ADR-0023`), refused
  until a named phase, or refused for good; `KANIKO_NO_PUSH` overrides the
  flag, `KANIKO_REGISTRY_MIRROR`/`_MAP` exit 2, `DOCKER_AUTH_CONFIG` is read
  before `config.json`, every other set `KANIKO_*`/`FF_KANIKO_*` variable
  gets one warning, and there is no `KANIKO_METRICS_FILE` alias. Recorded in
  [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md); the table is in
  [kaniko-map](agents/kaniko-map.md#flags).

### Q-031: How does per-stage log grouping fit the kaniko log flags?

- **Answer (2026-10-30):** option (a): `--verbosity`, `--log-format` and
  `--log-timestamp` take kaniko's meaning, and per-stage grouping stays a
  separate matter, independent of the format. Recorded in
  [ADR-0036](adr/0036-kaniko-flags-in-phase-1.md).

### Q-049: Which overlay optimizations come after Phase 1?

- **Answer (2026-10-30):** the maintainer chose option (a): the exporter
  leaves out every unchanged copy-up by BuildKit's full `sameDirent` rule,
  contents compared when both times are whole seconds, and writes an
  opaque directory as whiteouts of what it hides. Recorded in
  [ADR-0034](adr/0034-unchanged-copy-ups-left-out-of-layers.md). Keeping
  one mount for consecutive instructions moved to
  [Q-245](#q-245-should-one-overlay-mount-serve-consecutive-instructions).

### Q-001: Who owns the persistent store and `BuildPaths`?

- **Answer (2026-10-30):** a new crate, `rnk-store`, owns `blobs/`,
  `layers/`, `caches/`, `tmp/`, the per-digest locks, start-up cleanup,
  `prune` and `BuildPaths`. Recorded in
  [ADR-0021](adr/0021-rnk-store-crate.md).

### Q-002: What is the mode-selection flag, given kaniko's `--snapshot-mode`?

- **Answer (2026-10-30):** `--mode=auto|overlay|kaniko`;
  `--snapshot-mode=full|redo|time` keeps kaniko's meaning. Recorded in
  [ADR-0023](adr/0023-mode-flag.md).

### Q-003: Where does the store live in each mode?

- **Answer (2026-10-30):** one `--state-dir` for every mode, by default
  `<workdir>/.ranikust` in overlay mode and `/ranikust/state` in kaniko mode.
  Recorded in [ADR-0022](adr/0022-common-state-dir.md).

### Q-004: Which TLS stack does the registry client use?

- **Answer (2026-10-30):** rustls with the `ring` provider, trusting the
  host's CAs plus the embedded webpki roots. Recorded in
  [ADR-0024](adr/0024-tls-rustls-ring.md).

### Q-005: Which branch do feature branches start from?

- **Answer (2026-10-30):** the latest `main`. The repository set-up was
  merged into `main`; every feature, fix or refactor starts from it on its
  own branch and is committed at each checkpoint. Recorded in
  [rules.md](agents/rules.md#branches-and-commits).

### Q-050: Which result does the context walk follow when BuildKit's two matchers disagree?

- **Answer (2026-10-30):** follow BuildKit's walk
  (`matches_using_parent` plus `may_reinclude_below`). Recorded in
  [ADR-0027](adr/0027-context-filtering-follows-buildkit-walk.md), which
  supersedes the `.dockerignore` bullet of ADR-0015.

### Q-051: Does `FROM <stage>` match stage names case-insensitively?

- **Answer (2026-10-30):** follow BuildKit: `FROM <stage>` matches stage
  names exactly. kaniko's `Dockerfile_test_issue_519` is an accepted
  difference ([kaniko-map](agents/kaniko-map.md#behaviour-to-match)).

### Q-053: Which entry order and which names does the layer tar use?

- **Answer (2026-10-30):** follow the OCI layer specification's
  recommendation: depth first, the opaque marker and whiteouts before their
  siblings; files really named `.wh.*` are rejected. Recorded on the
  [rnk-layer card](agents/crates/rnk-layer.md) and in
  [overlay-snapshot](design/overlay-snapshot.md).

### Q-056: How are tar extension headers bounded?

- **Answer (2026-10-30):** no bound for now; GNU sparse entries stay
  refused. Revisit with `cargo fuzz` on `extract` in Phase 6
  ([rootfs](design/rootfs.md)).

### Q-052: How does zstd reach the static musl build?

- **Answer (2026-10-30):** cargo-zigbuild with zig in the `justfile` and
  CI, as [ADR-0020](adr/0020-static-musl-via-cargo-zigbuild.md) planned;
  the same toolchain builds mimalloc. zstd is added after it
  ([phase-1](agents/phase-1.md#task-order), task 5a).
- **Implemented (2026-10-30):** `just build-static` and `just
  check-aarch64` run cargo-zigbuild 0.24.0 with zig 0.16.0, the same in
  CI; zig compiles every C dependency (libzstd, mimalloc, ring) and the
  host `cc` links x86_64 to keep static-pie. `rnk-layer` writes zstd as
  independent 1 MiB frames.

### Q-054: What is the minimum kernel, and who checks it?

- **Answer (2026-10-30):** require Linux 5.8; the binary probes `openat2`
  and the `statx` mount id at start-up and refuses with guidance. Recorded
  in [ADR-0028](adr/0028-minimum-kernel-linux-5-8.md).

### Q-055: How does overlay-form extraction handle a hardlink into a lower layer?

- **Answer (2026-10-30):** option (b): copy the target's content from the
  lower layers' extracted directories as a regular file, which splits the
  hardlink as overlay copy-up does (`ADR-0012`). Recorded in
  [rootfs](design/rootfs.md#extraction); implemented with base-layer
  extraction (Phase 1 task 9).
- **Implemented (2026-10-30):** `rnk_rootfs::extract_overlay(dst, tar,
  lowers)` looks the target up in the lower layers as overlay's merged
  view (whiteouts, opaque directories) and copies it with its metadata
  and xattrs; `rnk-snapshot` still has to call it.

### Q-012: Which crates fill the Phase 1 dependency picks?

- **Answer (2026-10-30):** the picks the code already made: `rustix` for
  syscalls (with `libc` where `rustix` lacks a call), not `nix`; `sha2` for
  sha256, not `aws-lc-rs` (a C and CMake build, kept out by `ADR-0024`);
  gzip by hand on `flate2`/zlib-rs, not `gzp` (not deterministic across
  thread counts, `exp-05`); `reqwest`, the stack `oci-client` builds on,
  not `hyper` directly; no `docker_credential`: `rnk-registry` reads
  `config.json` itself and credential helpers come in Phase 5 (Q-047);
  `clap` (derive) for the CLI. None of them appears in a library's public
  API. Whether `ring::digest` beats `sha2` on large layers is measured
  under Q-033. Recorded in
  [ADR-0030](adr/0030-phase-1-dependency-picks.md).

### Q-075: Which host name does a RUN see?

- **Answer (2026-10-30):** a fixed `ranikust` (option c), whatever the
  environment, so a `RUN` that records its host name gives the same bytes
  on every host; no flag. `rnk-builder` may still set
  `RunSpec::hostname`. Implemented in `rnk-sandbox` (`DEFAULT_HOSTNAME`).
  Recorded in [ADR-0031](adr/0031-run-host-name-and-hosts-file.md).

### Q-076: What /etc/hosts does a RUN see?

- **Answer (2026-10-30):** option (c): with the host network, ranikust's
  own `/etc/hosts` (as kaniko's `RUN` sees it, so Kubernetes `hostAliases`
  keep working) plus the host name; with `--network=none`, loopback names
  plus the host name, as BuildKit. Implemented in `rnk-sandbox`
  (`plan.rs`, `hosts_file`). Recorded in
  [ADR-0031](adr/0031-run-host-name-and-hosts-file.md).

### Q-006: Which image format does ranikust write by default?

- **Answer (2026-10-30):** OCI media types by default, whatever the base
  image's format and `--reproducible`, with gzip layers; `--image-format`
  (Q-023) is the override. BuildKit's exporter defaults to OCI and converts
  base layers' media types. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-007: Which image-assembly types live in rnk-oci?

- **Answer (2026-10-30):** option (a): `ConfigEdit`,
  `ImageConfig::push_layer`/`push_history`, `DockerTarImage` and `OciLayout`
  stay pure types in `rnk-oci`; `rnk-builder` writes the files. An internal
  split with no BuildKit counterpart, so the interim is adopted.

### Q-009: Does the parser accept instructions deferred to Phase 5?

- **Answer (2026-10-30):** option (a): the parser accepts every instruction
  and flag BuildKit parses; the builder refuses what it cannot run yet,
  naming the phase. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-013: How does the layer writer learn missing parent metadata?

- **Answer (2026-10-30):** option (a): the writer takes each missing
  parent's metadata from the merged view, as containerd's `includeParents`
  does for BuildKit. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-014: Are cross-repository mounts part of Phase 1?

- **Answer (2026-10-30):** yes: push mounts blobs across repositories and
  falls back to an upload when the mount is refused, as containerd's pusher.
  Implemented in `rnk_builder::push`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-022: What format does the secret flag take?

- **Answer (2026-10-30):** the full buildctl/kaniko syntax: `id=`,
  `type=file|env`, `src=`/`source=`, `env=`; with no source, the environment
  variable named by the id if set, else the file named by the id; a secret
  is at most 500 KiB. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-025: Are other user xattrs kept in exported layers?

- **Answer (2026-10-30):** no: exported layers carry only
  `security.capability`, as BuildKit's (containerd's) layer writer. The
  entry's claim that BuildKit strips only other `user.*` xattrs was wrong.
  Recorded in [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-026: When and how are layers merged near the 500-lower limit?

- **Answer (2026-10-30):** no merging: a stage that needs more lowers than
  the kernel allows fails with a clear error, as BuildKit's overlay
  snapshotter. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-043: What are the prune defaults?

- **Answer (2026-10-30):** age by last use (option a), with BuildKit's tiers
  as defaults: cache mounts and contexts dropped after 48 h unused beyond
  512 MB, anything unused for 60 days, and size caps of 10% reserved, 20%
  minimum free, 80% maximum used. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-044: How do remote ONBUILD triggers reach the stage graph?

- **Answer (2026-10-30):** option (b): a separate step extends the graph
  once base configs are fetched, repeated until no new dependency appears,
  as BuildKit's `initOnBuildTriggers`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-046: What happens to HTTPS contexts on Azure Blob hosts?

- **Answer (2026-10-30):** option (b): fetch them as plain HTTPS, as
  BuildKit does for any `https://` context (an archive is unpacked, anything
  else is the Dockerfile). Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-065: Do localhost and private-network registries fall back to HTTP?

- **Answer (2026-10-30):** loopback registries (`localhost`, `127.0.0.0/8`,
  `::1`) use plain HTTP without a flag, as BuildKit; other hosts, RFC 1918
  and `*.local` included, fall back to HTTP only when listed with
  `--insecure-registry`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-077: How is the MS_MOVE fallback kept from a chroot escape?

- **Answer (2026-10-30):** option (b), the recommendation, confirmed by the
  VM escapes: `MS_MOVE` only where `/` is a mount root, keeping Docker's 14
  capabilities; ranikust inside a plain chroot refuses. Measured with `just
  test-vm smoke` on kernels 5.15 and 6.8
  ([testing](testing.md#the-vm-harness-tier-t2)).

### Q-080: Who resolves COPY sources against the context, and how?

- **Answer (2026-10-30):** option (a) with BuildKit's rules: `rnk-builder`
  resolves sources against the context and stage roots; a wildcard that
  matches nothing copies nothing and is not an error; there is no "several
  sources need a trailing `/`" rule. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-081: Does the context sub-path flag apply to a local directory?

- **Answer (2026-10-30):** option (a): the sub-path applies only to fetched
  (git, HTTPS, tar) contexts and is ignored for a local directory, as both
  BuildKit and kaniko do. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).
- **Answer (2026-10-31):** "Q81 theo đề xuất": drop kaniko's `dir://`
  context syntax (option (b) of the recommendation). A local context is a
  plain path, and `--context-sub-path` applies only to fetched contexts
  (git, HTTPS, tar), as the answer above says. The code still joined the
  sub-path onto a `dir://` context; that changes with the Phase 2 task 7
  integration branch. Recorded in
  [ADR-0053](adr/0053-local-context-is-a-plain-path-no-dir-scheme.md).
- **Answer (2026-10-31):** "Q81 A": the two points ADR-0053 left open.
  (1) `--context=dir://...` is refused with a message to use a plain path;
  (2) `--context-sub-path` with a local context is refused (exit 2) with a
  message to pass the sub-directory itself as `--context`, since a flag
  with no effect is refused, not ignored (ADR-0049). The code change lands
  with the Phase 2 task 7 integration branch. Recorded in
  [ADR-0054](adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md), which completes ADR-0053.

### Q-100: How durable is a published store entry after a power loss?

- **Answer (2026-10-30):** option (a): blobs are fsynced before publication;
  extracted layers are not synced, as containerd's defaults under BuildKit.
  Recorded in [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-111: Does the store open before or after the probes?

- **Answer (2026-10-30):** option (a), the recommendation: `set_up` opens
  the store (marker check, start-up clean-up) right after the state
  directory check and before both probes (`feat/rnk-snapshot`).

### Q-120: Where kaniko and BuildKit copy differently, which does `copy` follow?

- **Answer (2026-10-30):** BuildKit's copy, item by item: (1) a symlinked
  source is followed; (2) all xattrs are copied (layers still keep only
  `security.capability`, Q-025); (3) an existing top-level destination keeps
  its metadata, existing directories below it take the source's; (4) devices
  and FIFOs are recreated and sockets become empty regular files; (5)
  `--chmod` applies to directories too; (6) symbolic `--chmod` (`u+x`) is
  accepted. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-121: Which overlay forms does the lower-layer lookup recognise?

- **Answer (2026-10-30):** option (a), the recommendation: only ranikust's
  own form (`user.overlay.opaque=y`, 0/0 whiteouts), `layers/` being written
  by ranikust alone.

### Q-130: Where kaniko and BuildKit differ on a config edit, which one wins?

- **Answer (2026-10-30):** BuildKit for each: (1) a new `ENTRYPOINT` clears
  an inherited `CMD` unless the stage set one; (2) `EXPOSE` takes
  `tcp|udp|sctp` in any case, writes it in lower case and expands ranges
  into one key per port; (3) `HEALTHCHECK --start-interval` is kept; (4) an
  absolute `WORKDIR` stays as written, which BuildKit does too (the entry's
  claim that it cleans it was wrong). Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-131: Which instructions get a history entry, with which fields?

- **Changed (2026-11-01):** a step that changes nothing adds no layer,
  only its history entry with `empty_layer: true`
  ([ADR-0058](adr/0058-a-step-that-changes-nothing-adds-no-layer.md), Q-385).

- **Answer (2026-10-30):** BuildKit's: one entry per instruction,
  `empty_layer: true` for metadata instructions, a layer for `RUN`, `COPY`,
  `ADD` and `WORKDIR` other than `/`, no entry for `FROM` and `ONBUILD`, no
  author; `RUN`'s `created_by` carries the build args. The `comment` string
  is the implementing branch's choice. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-132: Are empty values of a base config written back?

- **Answer (2026-10-30):** option (b), plus BuildKit's spec round-trip:
  empty strings, `false`, empty lists and maps are dropped, and so are
  fields outside the OCI and Docker image specs (`Hostname`, `Domainname`,
  `Tty`, `AttachStd*`, `OpenStdin`, `StdinOnce`, `Image`, `container`,
  `container_config`, `docker_version`). Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-150: Is an extracted layer keyed by its diff ID alone?

- **Answer (2026-10-30):** option (b): a layer whose extraction copied a
  hardlink target from a lower is keyed by its diff ID and the chain below
  it; other layers keep the diff-ID key. BuildKit keys every unpacked layer
  by chain ID. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-151: How does the mount(2) fallback fit many layers?

- **Answer (2026-10-30):** option (b), the containerd way: mount from a
  short-lived thread that does `unshare(CLONE_FS)` and `chdir` into `l/`,
  with relative lower names; no child process. The entry's "only in a forked
  child" was wrong. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-160: Are base layers rewritten under `--reproducible`?

- **Answer (2026-10-30):** option (a): base layers are kept as pulled, and
  their history keeps its times; only the layers built after the base are
  rewritten, as BuildKit's `rewriteRemoteWithEpoch`. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-161: Which fixed time does `--reproducible` write?

- **Answer (2026-10-30):** option (a): `SOURCE_DATE_EPOCH` when set, else
  the Unix epoch (equal to `SOURCE_DATE_EPOCH=0`). As BuildKit, times above
  that epoch are clamped to it and older ones are kept, instead of every
  time being set. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-180: Does VOLUME create its directories?

- **Answer (2026-10-30):** option (a): `VOLUME` only records the config, and
  an empty name is refused, as BuildKit. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-181: How is `RUN --mount=type=cache,sharing=` enforced?

- **Answer (2026-10-30):** enforce `sharing=` as BuildKit: `shared` is one
  cache for all; `locked` waits until the cache is free; `private` takes a
  new empty cache when the existing one is busy (not a copy). `uid`, `gid`
  and `mode` apply when the cache is created and are part of its key. The
  entry's "private = own copy" was wrong. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-182: Which `RUN --mount=type=secret` options are honoured?

- **Answer (2026-10-30):** a build-private copy of the secret, mode `0400`
  owned by root by default, with `mode`, `uid` and `gid` honoured, and
  `env=` supported, as BuildKit. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-184: May a Dockerfile's `RUN --network=host` lift `--network=none`?

- **Answer (2026-10-30):** no, unless the operator allows it: `RUN
  --network=host` needs `--allow network.host` (BuildKit's entitlement),
  which then lifts `--network=none`. Under the default host network
  (`ADR-0010`) nothing extra is needed. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-185: Does an image without `PATH` get one in its config?

- **Answer (2026-10-30):** yes: any Linux config without `PATH` gets
  `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, as
  BuildKit; this replaces the interim that added it only for `scratch`.
  Recorded in [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-186: Does a cancel kill a running `RUN`?

- **Answer (2026-10-30):** yes: a cancel kills the running `RUN` with
  SIGKILL (its PID 1, and with it the PID namespace), as BuildKit's
  executor. Recorded in
  [ADR-0032](adr/0032-observable-behaviour-follows-buildkit.md).

### Q-187: How does the W3 digest check treat the workload's `du` output?

- **Answer (2026-10-30):** option (b): the acceptance criterion compares
  `--jobs 1` with the default (equal on 2026-10-30); the e2e suite compares
  the W3 images across `--jobs` by flattened content with the workload's
  `du`-derived files on its noise list, because those files depend on disk
  block allocation, not on ranikust. Every W3 stage writes `du`, so the
  list is `/out/a.txt`, `/out/b.txt`, `/out/c.txt` and `/out/all.txt`
  (`/out/b.txt` was seen to differ too; corrected on the branch that made
  the harness change).
  `nextjs-kaniko` is a separate project and is not changed here.

### Q-200: How does the e2e suite's `ranikust` reach a plain-HTTP registry?

- **Answer (2026-10-30):** option (a), the recommendation:
  `--insecure-registry <host>` (repeatable) with kaniko's meaning
  (`ADR-0011`), HTTPS first and plain HTTP when it fails
  (`feat/rnk-builder-executors`).

### Q-045: How are git contexts fetched and addressed?

- **Answer (2026-10-30):** option (c): a built-in `gix` backend by default,
  an external `git` with `--git-backend=external`, its binary, extra
  configuration (`--git-config`, passed through `GIT_CONFIG_*`), extra
  global options (`--git-arg`) and the host configuration
  (`--git-host-config`) configurable; kaniko's `--git` and `GIT_*`
  variables kept; the external `git` isolated and hardened as BuildKit's,
  plus hooks and `fsmonitor` off; both fragments, `#ref:subdir` and
  `#ref#commit`. Decided by the maintainer; recorded in
  [ADR-0035](adr/0035-git-contexts-built-in-or-external-git.md).

### Q-183: What does `RUN --network=none` do without `CAP_NET_ADMIN`?

- **Answer (2026-10-30):** (a) by default: refused where the build
  process cannot bring up `lo`; `--allow network.none-userns` opts in to
  (c), a network namespace owned by a user namespace of its own, with a
  seccomp filter on the `RUN` (a denylist; rtnetlink stays open), since its
  root holds every capability in that user namespace. Phase 3 moves the opt-in to running the `RUN`
  inside the user namespace. Recorded in
  [ADR-0033](adr/0033-network-none-loopback-through-a-user-namespace.md).

### Q-341: May kaniko mode build on `/` in a container where root may mount?

- **Answer (2026-10-31):** "Q341 chọn A": option (a). Kaniko mode on `/` is
  refused where root may mount, and also where `/proc/sys` is not
  read-only (`statvfs`), pointing to overlay mode; host devices given with
  `--device` stay the operator's responsibility (documented). Option (b),
  a private mount namespace with Docker's read-only and masked paths, is
  not built. Recorded in [ADR-0054](adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md).
