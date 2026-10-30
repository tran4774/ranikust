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

### Q-321: Is the container check enough to guard a kaniko-mode build on `/`?

- **Blocks:** nothing yet; **Area:** `ranikust`
- **Context:** kaniko mode without `--workdir` builds on `/`: it extracts
  the base over it and empties it between stages. Outside a container only
  `--force` lets it run (ADR-0040), and whether ranikust is in a container
  is kaniko's heuristic (`container::detect`: marker files, cgroup names,
  the `container` variable, an overlay `/`), which a host can match by
  accident (an overlay root, a leftover `/.dockerenv`).
- **Options:** (a) keep kaniko's heuristic and `--force`, as kaniko; (b)
  also require an explicit flag naming `/` (for example `--workdir=/`) when
  the heuristic is the only evidence; (c) a first-run confirmation, which
  does not fit an unattended CI job.
- **Recommendation:** (b), under the maintainer's priorities (security
  first, then performance; kaniko drop-in parity is not a goal): require
  an explicit opt-in to build on `/` (`--workdir=/`, or `--force` as the
  only switch) and never trust the container heuristic alone before
  extracting over or emptying `/`. Option (a), kaniko's behaviour, is
  weaker: a host that happens to match the heuristic loses its `/`
  without any flag.
- **Interim:** (a), unchanged until the maintainer decides.
- **Raised:** 2026-10-31

### Q-008: What does a repeated target flag mean?

- **Blocks:** Phase 1 tasks 2, 8; **Area:** `rnk-dockerfile`, `ranikust`
- **Context:** kaniko's `--target` is a list whose first stage is pushed
  (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:333`;
  [kaniko-map](agents/kaniko-map.md#flags)); `stage_graph()` takes one
  `Option<&str>` ([rnk-dockerfile](agents/crates/rnk-dockerfile.md#api-planned)).
- **Options:** (a) kaniko's list: build every listed stage, push the first;
  (b) one target, a repeated flag rejected with a message (`ADR-0011`).
- **Recommendation:** (b), one target: the stages after the first in
  kaniko's list are built and thrown away, work no output needs;
  compatibility with kaniko's list is not a goal (2026-10-31).
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
- **Recommendation:** (b), bytes, as the interim: a file whose name is
  not UTF-8 must still match an exclude, or it slips into the context
  (a secret, a key) unseen (2026-10-31).
- **Interim:** bytes, on branch `feat/rnk-ignore-matcher`: patterns and
  paths are `impl AsRef<[u8]>` (so `&str` callers need no change) and
  `read_patterns` returns `Vec<Vec<u8>>`. Go strings are bytes too, so this
  is the only choice that matches `patternmatcher` on non-UTF-8 names:
  regexps see an invalid byte as U+FFFD, as Go's do (matched with `regex`
  on the decoded text rather than `regex::bytes`). Reversible: the crate is
  unpublished and has no callers yet; narrowing to `&str` changes only
  signatures.
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
- **Recommendation:** (a): a lock race or a crash mid-extraction can
  publish a partial layer that later builds trust; proven when the store
  is built, not left for Phase 6 (2026-10-31).
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
- **Recommendation:** (a): an explicit `--workdir` or `--state-dir`, never
  an implicit `/` chosen by a heuristic, and never a temporary directory
  that loses the store between builds; the design's container default
  was for kaniko compatibility, which is not a goal (2026-10-31).
- **Interim:** (a), on `feat/ranikust-setup`: without either flag, overlay
  mode (and `auto`, whose only Phase 1 outcome is overlay) stops with a
  usage error, exit code 2. Reversible: one check in `main.rs`.
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
- **Recommendation:** (b): a built layer published once in the store is
  reused by later builds and the cache instead of encoded again; the
  digest is verified as it is written (2026-10-31).
- **Interim:** (a), on branch `feat/rnk-builder-outputs`. Reversible: the
  exporter's directory and one rename.
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
  ([kaniko-mode](design/kaniko-mode.md#open-questions)).
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
  `#ref:subdir#commit`; (c) use `--context-sub-path` only; (d) drop
  kaniko's `#ref#commit` form and accept BuildKit's `#ref:subdir` only,
  pinning by a full commit hash as the ref.
- **Recommendation:** (d): one fragment grammar, so no URL is read two
  ways; a full commit hash pins the source as kaniko's second part did.
  Accepting kaniko's form only served compatibility (2026-10-31).
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
  names; (c) names chosen with the semantics, set first by which
  credentials each registry receives.
- **Recommendation:** (c): decide first that a mirror or mapped registry
  never receives another registry's credentials and never serves a push,
  then name the flags; kaniko's names only where its meaning is exactly
  that. Compatibility is not a goal (2026-10-31).
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
- **Recommendation:** `push` input (b), an OCI layout only: one parser
  for untrusted input instead of two, and the layout is what
  `--oci-layout-path` writes; the docker-save tar only served kaniko
  compatibility. `login` not at all: it writes credentials to disk, which
  a tool that reads existing ones does not need. Phase: none (2026-10-31).
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
- **Recommendation:** (a), behind a measurement: time the mount and
  unmount per instruction on W1–W3; build it if it is a visible share of
  a build, with the `volatile` rules unchanged (2026-10-31).
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

### Q-260: Should overlay mode honour kaniko's snapshot flags?

- **Blocks:** nothing yet; **Area:** `ranikust`, `rnk-snapshot`
- **Context:** `--ignore-path`, `--single-snapshot` and
  `--ignore-var-run=false` change the image kaniko builds. Kaniko mode
  takes them ([ADR-0040](adr/0040-kaniko-mode-selection-container-check-and-flags.md));
  overlay mode could too (filter the upper directory's diff, squash the
  layers, keep `/var/run`), but nothing is designed.
- **Options:** (a) refuse them in overlay mode; (b) implement them in
  overlay mode's exporter; (c) ignore them with a warning (breaks
  `ADR-0036`'s rule that a flag changing the image is never dropped);
  (d) refuse them in overlay mode for good, and in kaniko mode keep only
  what safety or performance needs.
- **Recommendation:** (d): overlay mode needs none of them: its layers
  hold only what `RUN` wrote, never container mounts or `/var/run`, and
  squashing costs a full re-encode for no safety gain. In kaniko mode
  mounts are already ignored through mountinfo, so `--ignore-path` and
  `--ignore-var-run=false` stay only if a case needs them;
  `--single-snapshot` only if measured faster. They existed for kaniko
  compatibility, which is not a goal (2026-10-31).
- **Interim:** (a): exit code 2 naming the flags and `--mode=kaniko`.
  Reversible: one function, `config::overlay_mode_flags`.
- **Raised:** 2026-10-31

## Answered

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

### Q-183: What does `RUN --network=none` do without `CAP_NET_ADMIN`?

- **Answer (2026-10-30):** (a) by default: refused where the build
  process cannot bring up `lo`; `--allow network.none-userns` opts in to
  (c), a network namespace owned by a user namespace of its own, with a
  seccomp filter on the `RUN` (a denylist; rtnetlink stays open), since its
  root holds every capability in that user namespace. Phase 3 moves the opt-in to running the `RUN`
  inside the user namespace. Recorded in
  [ADR-0033](adr/0033-network-none-loopback-through-a-user-namespace.md).
