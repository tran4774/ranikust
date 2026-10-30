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

### Q-008: What does a repeated target flag mean?

- **Blocks:** Phase 1 tasks 2, 8; **Area:** `rnk-dockerfile`, `ranikust`
- **Context:** kaniko's `--target` is a list whose first stage is pushed
  (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:333`;
  [kaniko-map](agents/kaniko-map.md#flags)); `stage_graph()` takes one
  `Option<&str>` ([rnk-dockerfile](agents/crates/rnk-dockerfile.md#api-planned)).
- **Options:** (a) kaniko's list: build every listed stage, push the first;
  (b) one target, a repeated flag rejected with a message (`ADR-0011`).
- **Recommendation:** none
- **Interim:** in `feat/rnk-dockerfile-parser`, `stage_graph()` takes
  `GraphOptions::targets: Vec<String>`: every listed stage is built, the
  first is the result (`StageGraph::primary_target`), and `TARGETSTAGE` is
  the first name. Reversible because a single target is a one-element list:
  option (b) only needs the CLI (task 8) to reject a repeated flag. The CLI
  (`feat/ranikust-cli`) follows: `--target` takes a list, comma-separated
  or repeated, an empty name refused.
- **Raised:** 2026-10-30

### Q-010: Which path type does rnk-ignore match on?

- **Blocks:** Phase 1 task 3; **Area:** `rnk-ignore`
- **Context:** the planned `Matcher` takes `&str`, which cannot hold
  non-UTF-8 file names; the card says to decide the path type first
  ([rnk-ignore](agents/crates/rnk-ignore.md#api),
  [Phase 1](agents/crates/rnk-ignore.md#phase-1)).
- **Options:** (a) `&str`; (b) bytes, matched with `regex::bytes`.
- **Recommendation:** none
- **Interim:** bytes, on branch `feat/rnk-ignore-matcher`: patterns and
  paths are `impl AsRef<[u8]>` (so `&str` callers need no change) and
  `read_patterns` returns `Vec<Vec<u8>>`. Go strings are bytes too, so this
  is the only choice that matches `patternmatcher` on non-UTF-8 names:
  regexps see an invalid byte as U+FFFD, as Go's do (matched with `regex`
  on the decoded text rather than `regex::bytes`). Reversible: the crate is
  unpublished and has no callers yet; narrowing to `&str` changes only
  signatures.
- **Raised:** 2026-10-30

### Q-011: Who owns the workdir checks, and what counts as empty?

- **Blocks:** Phase 1 tasks 4, 8; **Area:** `rnk-rootfs`, `ranikust`,
  `rnk-store`
- **Context:** `check_workdir` sits in `rnk-rootfs` "here or `ranikust`"
  ([rnk-rootfs](agents/crates/rnk-rootfs.md#api-planned),
  [ranikust](agents/crates/ranikust.md#must-not)); the owner of the
  kaniko-mode workdir lock is open too
  ([rnk-store](agents/crates/rnk-store.md#phase-1)).
  A workdir holding only mount points has no marker and is refused
  ([kaniko-mode](design/kaniko-mode.md#open-questions)).
- **Options:** owner (a) `rnk-rootfs` or (b) `ranikust`; a workdir of pure
  mount points (c) counts as empty or (d) is refused.
- **Recommendation:** none
- **Interim:** a workdir of pure mount points is refused, as designed today.
  Owner: `feat/rnk-rootfs-rootpath` does not build `check_workdir` in
  `rnk-rootfs`; it only provides the primitives either owner needs
  (`RootPath::open` with `RESOLVE_NO_SYMLINKS`, `statx` with `dev`, `ino`
  and `mnt_id`). Reversible because nothing calls a workdir check yet.
  `feat/ranikust-cli` (task 8a) does not check the workdir either: it only
  makes `--workdir` and `--state-dir` absolute, without touching the
  filesystem; the build stops before any use of them.
- **Raised:** 2026-10-30

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
- **Recommendation:** none
- **Interim:** none
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
- **Options:** (a) under the overlay-mode default `<workdir>/.ranikust`;
  (b) in a probe-only temporary directory in the workdir.
- **Recommendation:** none
- **Interim:** (a), because Phase 1 has no outcome but overlay mode:
  `feat/ranikust-setup` probes in the overlay-mode state directory
  (`--state-dir` or `<workdir>/.ranikust`); the sandbox probe uses its
  `sandbox/` subdirectory, and the overlay probe (`feat/rnk-snapshot`)
  this build's `builds/<id>/overlay-probe`, deleted with the build.
  Revisit with kaniko mode (Phase 2).
- **Raised:** 2026-10-30

### Q-019: What is the final name of the parallel-stages flag?

- **Blocks:** Phase 1 task 8; **Area:** `ranikust`
- **Context:** `--parallel-stages` is a placeholder, fixed with the flag
  list ([ADR-0011](adr/0011-accept-common-kaniko-flags.md#decision-outcome),
  [ADR-0014](adr/0014-kaniko-mode-sequential-by-default.md#decision-outcome),
  [ADR-0023](adr/0023-mode-flag.md#more-information),
  [phase-1.md](agents/phase-1.md#open-questions-blocking-phase-1)); the flag
  itself lands in Phase 2 ([ranikust](agents/crates/ranikust.md#phase-1)).
- **Options:** (a) keep `--parallel-stages`; (b) another name.
- **Recommendation:** none
- **Interim:** the placeholder `--parallel-stages`.
- **Raised:** 2026-10-30

### Q-020: Is custom-platform accepted as an alias of platform?

- **Blocks:** Phase 1 task 8; **Area:** `ranikust`
- **Context:** `ADR-0009` gives ranikust `--platform`, `linux/amd64` only at
  first; whether kaniko's `--custom-platform` is an alias is open
  ([kaniko-map](agents/kaniko-map.md#flags),
  [phase-1.md](agents/phase-1.md#open-questions-blocking-phase-1)).
- **Options:** (a) accept `--custom-platform` as an alias; (b) reject it
  with a message naming `--platform`.
- **Recommendation:** none
- **Interim:** option (b), in `feat/ranikust-cli`: `--custom-platform`
  and the deprecated `--customPlatform` are refused with a message naming
  `--platform`. Reversible: an alias is one clap attribute.
- **Raised:** 2026-10-30

### Q-021: What do the cache flags do before Phase 4?

- **Blocks:** Phase 1 task 8; **Area:** `ranikust`, `rnk-cache`
- **Context:** `ADR-0011` accepts `--cache` and `--cache-repo`, but the
  layer cache is Phase 4 ([rnk-cache](agents/crates/rnk-cache.md#phase-1),
  [kaniko-map](agents/kaniko-map.md#flags),
  [phase-1.md](agents/phase-1.md#open-questions-blocking-phase-1)). An
  accepted kaniko flag keeps its meaning or is rejected with a message
  ([ranikust](agents/crates/ranikust.md#invariants)).
- **Options:** (a) reject both with a clear message until Phase 4;
  (b) accept them as no-ops.
- **Recommendation:** none
- **Interim:** option (a), in `feat/ranikust-cli`: `--cache` and
  `--cache-repo` are refused with a message naming Phase 4, also
  `--cache=false`. Reversible: accepting them is two flags.
- **Raised:** 2026-10-30

### Q-023: Which other kaniko flags and variables are accepted?

- **Blocks:** Phase 1 tasks 8, 13; **Area:** `ranikust`
- **Context:** about 30 kaniko flags are still to be decided with the list
  ([kaniko-map](agents/kaniko-map.md#flags): `--verbosity`, `--log-format`,
  `--label`, `--image-format`, `--digest-file`, `--insecure`,
  `--insecure-pull` (`--insecure-registry` is accepted, Q-200),
  `--push-retry`, `--single-snapshot`, `--cleanup`, `--dryrun`, deprecated
  forms…), plus `--force` (replaced by the workdir checks), `--cache-dir`
  (the store replaces it) and `KANIKO_REGISTRY_MIRROR`, `KANIKO_NO_PUSH`,
  `KANIKO_REGISTRY_MAP` ([ranikust](agents/crates/ranikust.md#pitfalls)).
  `ADR-0011` and `ADR-0023` fix the list in Phase 1. Also
  `KANIKO_METRICS_FILE` (the maintainer's kaniko fork,
  `kaniko@88b6d2b75`): honour it as an alias of `RANIKUST_METRICS_FILE`,
  whose report follows the same sections but not the same phase names
  (`ADR-0025`, [metrics](design/metrics.md#comparing-with-kaniko)), and the
  final name of the `--metrics-file` flag.
- **Options:** per flag, (a) accept with kaniko's meaning and a test;
  (b) reject with a message; (c) accept and ignore with a warning, as
  `--snapshot-mode` in overlay mode (`ADR-0023`).
- **Recommendation:** none
- **Interim:** `KANIKO_METRICS_FILE` is not read; only
  `RANIKUST_METRICS_FILE` turns metrics on (`feat/rnk-metrics`). Reversible:
  an alias is one more variable in `rnk_metrics::Config::from_env`. In
  `feat/ranikust-cli`: option (b) for every flag listed here, `--force` and
  `--cache-dir` included; each is refused with exit code 2 and a reason
  (`crates/ranikust/src/cli.rs` `REFUSED`), the deprecated forms naming
  their replacement. The flag keeps the name `--metrics-file` and wins over
  the variable. `KANIKO_NO_PUSH`, `KANIKO_REGISTRY_MIRROR` and
  `KANIKO_REGISTRY_MAP` are not read. Reversible: accepting a flag removes
  a table row.
- **Raised:** 2026-10-30

### Q-110: What is the overlay-mode state directory without `--workdir`?

- **Blocks:** Phase 1 task 8; **Area:** `ranikust`
- **Context:** the overlay-mode default is `<workdir>/.ranikust`
  (ADR-0022), and `--workdir` defaults to `/` only inside a container
  ([modes-and-detection](design/modes-and-detection.md#workdir-rules)),
  which needs a container check that is not built (Q-011). In a container,
  `/.ranikust` usually sits on the container's overlay root, which cannot
  hold an upper directory, so the overlay probe would fail there anyway.
- **Options:** (a) require `--workdir` or `--state-dir` in overlay mode;
  (b) default the workdir to `/` in a container, as designed, and let the
  probe refuse; (c) a temporary directory.
- **Recommendation:** none
- **Interim:** (a), on `feat/ranikust-setup`: without either flag, overlay
  mode (and `auto`, whose only Phase 1 outcome is overlay) stops with a
  usage error, exit code 2. Reversible: one check in `main.rs`.
- **Raised:** 2026-10-30

### Q-024: What is the Snapshotter signature?

- **Blocks:** Phase 1 task 9; **Area:** `rnk-snapshot`
- **Context:** `ADR-0002` names the trait; its signature is designed in
  task 9 ([rnk-snapshot](agents/crates/rnk-snapshot.md#api),
  [glossary](agents/glossary.md#terms),
  [ARCHITECTURE.md](../ARCHITECTURE.md#rnk-snapshot)).
- **Options:** the card's sketch (`prepare(base, parent)`, `final_root`),
  or the shape task 9 arrives at.
- **Recommendation:** none
- **Interim:** task 9's shape (`feat/rnk-snapshot`):
  `Snapshotter::{base_layer, prepare(Base::Scratch | Image | Stage)}` and
  `StageRootfs::{root, mount_point, begin_layer, end_layer -> LayerDiff,
  view, release}`; `final_root` became `view` (a read-only mount between
  layers), and `sharing` waits for kaniko mode (Phase 2). Reversible: one
  implementation, and no caller until `rnk-builder` (task 11).
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
  `ranikust` → `rnk-registry` for `login` and `push` (Q-048).
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

### Q-162: Where do the blobs of built layers live?

- **Blocks:** nothing in Phase 1; Phase 4 (cache); **Area:** `rnk-builder`,
  `rnk-store`
- **Context:** `rnk_store::Store::fill` takes the digest before the content
  is written, which suits pulled blobs; a built layer's digest is known only
  after encoding. `LayerExporter` therefore encodes into the build
  directory, where the blob is deleted with the build.
- **Options:** (a) the build directory; (b) a store call that publishes a
  temporary file under the digest computed while writing it, so later
  builds and the cache reuse it.
- **Recommendation:** none
- **Interim:** (a), on branch `feat/rnk-builder-outputs`. Reversible: the
  exporter's directory and one rename.
- **Raised:** 2026-10-30

### Q-183: What does `RUN --network=none` do without `CAP_NET_ADMIN`?

- **Blocks:** Phase 1 acceptance (`--network=none`); Phase 3;
  **Area:** `rnk-sandbox`
- **Context:** the sandbox brings up loopback in the `RUN`'s new network
  namespace, which needs `CAP_NET_ADMIN` over it. In a container with
  Docker's or podman's default capabilities plus `SYS_ADMIN` (the
  `MS_MOVE` environment) that step fails with `EPERM`, measured with the
  e2e build in rootless podman; `--privileged` and real root work.
- **Options:** (a) refuse, as now (`RUN sandbox: bring up the loopback
  interface: Operation not permitted`); (b) run with loopback down when
  it cannot be brought up, with a warning; (c) create the network
  namespace inside a user namespace, whose root may bring it up.
- **Recommendation:** none
- **Interim:** (a); the VM's `SYS_ADMIN` variant adds `NET_ADMIN` for
  the e2e build. Reversible: one step of the child's set-up.
- **Escalated:** to the maintainer under the ADR-0032 rule (B1: BuildKit
  always has loopback up through runc, inside a user namespace when
  rootless, while the `MS_MOVE` environment may deny `CAP_NET_ADMIN` and
  user namespaces).
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

### Q-031: How does per-stage log grouping fit the kaniko log flags?

- **Blocks:** Phase 1 task 12; **Area:** `rnk-sched`, `ranikust`
- **Context:** lines carry `[<stage-name> <k>/<n>]` and an option groups
  output per stage ([scheduler](design/scheduler.md#logging)); kaniko's
  `--verbosity`, `--log-format` and `--log-timestamp` are still undecided
  (Q-023). How the two interact is open
  ([scheduler](design/scheduler.md#open-questions)).
- **Options:** (a) grouping as its own option, independent of the format;
  (b) grouping as a value of `--log-format`.
- **Recommendation:** none
- **Interim:** no grouping yet; `rnk-sched` hands each stage its
  `LogTag` (`[<stage-name> <k>/<n>]`), which either option uses.
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
  ([kaniko-mode](design/kaniko-mode.md#open-questions)).
- **Options:** none listed.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-034: When must a lazily skipped stage still be snapshotted?

- **Blocks:** Phase 2; **Area:** `rnk-snapshot`, `rnk-sched`
- **Context:** kaniko mode does not snapshot a stage used only through
  `COPY --from` or `FROM <stage>`; a stage skipped this way that later needs
  a layer must still detect deletions, and `--cache` changes which layers
  are needed ([kaniko-mode](design/kaniko-mode.md#lazy-snapshots),
  [open questions](design/kaniko-mode.md#open-questions),
  [rnk-sched](agents/crates/rnk-sched.md#pitfalls)).
- **Options:** none listed.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-035: What does the kaniko-mode ignore list hold?

- **Blocks:** Phase 2; **Area:** `rnk-snapshot`, `rnk-rootfs`
- **Context:** besides mount points, `--ignore-path`, `/proc`, `/sys`,
  `/dev` and the state directory: all of `/ranikust` (an image may keep the
  binary there, as kaniko keeps `/kaniko/executor`) or only
  `/ranikust/state` ([kaniko-mode](design/kaniko-mode.md#open-questions));
  and kaniko's `/etc/mtab`, `/tmp/apt-key-gpghome` and `/var/run`
  ([kaniko-map](agents/kaniko-map.md#behaviour-to-match)).
- **Options:** (a) all of `/ranikust` or (b) only `/ranikust/state`;
  (c) add kaniko's three paths or (d) leave them out.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-036: What is the state directory default for kaniko mode on a host?

- **Blocks:** Phase 2; **Area:** `rnk-store`, `ranikust`
- **Context:** the kaniko-mode default `/ranikust/state` fits a container;
  for kaniko mode outside one (per-stage directories on a host) the default
  is open ([ADR-0022](adr/0022-common-state-dir.md#more-information),
  [cache-and-store](design/cache-and-store.md#open-questions),
  [overview](agents/overview.md#where-state-lives)).
- **Options:** (a) keep `/ranikust/state`; (b) require `--state-dir`.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-037: What else must the fake proc cover?

- **Blocks:** Phase 2; **Area:** kaniko mode, `--parallel-stages`
- **Context:** on Kubernetes with cgroup v1 and the limit only on a parent
  cgroup, the fake `/proc` may need `memory.stat` as a fifteenth file;
  long-running `RUN`s see stale values; other `$ORIGIN` users (conda, .NET)
  and cargo, go and Python builds were not tried on this path
  ([kaniko-mode](design/kaniko-mode.md#open-questions)).
- **Options:** (a) add `memory.stat`; (b) keep fourteen files until a case
  fails. The other items need tests.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-038: Which crates fill the Phase 2 dependency picks?

- **Blocks:** Phase 2; **Area:** dependencies
- **Context:** survey 03 §3 leaves the snapshot hash and the parallel walk
  open ([phase-1.md](agents/phase-1.md#open-questions-blocking-phase-1) O5,
  [refs.md](agents/refs.md#crates)); the hash choice barely matters
  ([kaniko-mode](design/kaniko-mode.md#walk-hash-and-diff)).
- **Options:** hash: (a) `highway` or (b) `xxhash-rust`; walk: (c) `jwalk`
  or (d) `rayon` + `getdents64`.
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
- **Recommendation:** none
- **Interim:** none
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
  `#ref:subdir#commit`; (c) use `--context-sub-path` only.
- **Recommendation:** none
- **Interim:** none
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

### Q-047: What are the mirror, registry-map and helper flags called?

- **Blocks:** Phase 5; **Area:** `ranikust`, `rnk-registry`
- **Context:** mirrors and credential helpers land in Phase 5; the names
  for kaniko's `--registry-mirror`, `--registry-map` and
  `--credential-helpers` are open ([kaniko-map](agents/kaniko-map.md#flags)).
- **Options:** (a) kaniko's names and meanings (`ADR-0011`); (b) other
  names.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-048: When do the push and login subcommands come?

- **Blocks:** nothing yet; **Area:** `ranikust`, `rnk-registry`
- **Context:** `ADR-0011` names `push` and `login`, but no phase has them
  ([ranikust](agents/crates/ranikust.md#phase-1),
  [rnk-registry](agents/crates/rnk-registry.md#phase-1)). kaniko's `push`
  takes a docker-save tar or an OCI layout
  ([ranikust](agents/crates/ranikust.md#api-planned)); `login` writes
  `config.json`, which `docker_credential` cannot
  ([rnk-registry](agents/crates/rnk-registry.md#pitfalls)).
- **Options:** phase: none listed; `push` input: (a) both formats, as
  kaniko; (b) one format.
- **Recommendation:** none
- **Interim:** `feat/ranikust-cli` has `build` and `version` only;
  `ranikust push` and `ranikust login` are clap errors (exit code 2).
- **Raised:** 2026-10-30

### Q-245: Should one overlay mount serve consecutive instructions?

- **Blocks:** nothing yet; **Area:** `rnk-snapshot`
- **Context:** each instruction mounts and unmounts its own overlay; one
  mount for several consecutive instructions would save that cost, against
  the `volatile` remount rules
  ([overlay-snapshot](design/overlay-snapshot.md#the-volatile-lifecycle)).
  BuildKit mounts per exec, so it has no counterpart. Split out of Q-049.
- **Options:** (a) build it after Phase 1; (b) drop it.
- **Recommendation:** none
- **Interim:** none
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

### Q-140: How is the aarch64 binary made static-pie with symbols?

- **Blocks:** an aarch64 release (Phase 6); **Area:** build, CI
- **Context:** `ADR-0020` releases a static-pie binary with its symbol
  table; x86_64 gets both by linking with the host `cc`. For
  `aarch64-unknown-linux-musl` there is no host cross `cc`: zig's linker
  (the cargo-zigbuild default) gives a static `EXEC` without `.symtab`, and
  `rust-lld` (`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`)
  a static `EXEC` with `.symtab`; both run `ranikust version` under
  qemu-user (measured on branch `ci/aarch64-tests`). `just test-aarch64`
  checks only that the binary is static.
- **Options:** (a) `rust-lld` plus flags that give static-pie, if rustc's
  target allows it; (b) a musl cross `cc` (e.g. `aarch64-linux-musl-gcc`)
  as the linker in CI; (c) accept a non-PIE aarch64 binary and amend the
  ADR.
- **Recommendation:** none
- **Interim:** zig's linker, static `EXEC`, no symbols; nothing is released
  for aarch64 (`ADR-0009`).
- **Raised:** 2026-10-30

## Answered

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
