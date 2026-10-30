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

### Q-006: Which image format does ranikust write by default?

- **Blocks:** Phase 1 tasks 1, 11; **Area:** `rnk-oci`, `rnk-builder`
- **Context:** `ImageFormat`'s default is open
  ([rnk-oci](agents/crates/rnk-oci.md#api)): kaniko's
  `--image-format` follows the base image
  ([pitfalls](agents/crates/rnk-oci.md#pitfalls)), and under
  `--reproducible` kaniko switches to Docker v2s2 unless
  `FF_KANIKO_REPRODUCIBLE_PRESERVE_FORMAT`
  ([kaniko-map](agents/kaniko-map.md#behaviour-to-match)). zstd needs OCI.
- **Options:** (a) follow kaniko: the base's format, Docker v2s2 under
  `--reproducible`; (b) the base's format in both cases; (c) OCI unless
  `--image-format` says otherwise.
- **Recommendation:** none
- **Interim:** `rnk-oci` defines `ImageFormat { Oci, Docker }` without a
  `Default` (branch `feat/rnk-oci-types`, marked `[open: Q-006]` in
  `media_type.rs`): the caller passes the format, so nothing in `rnk-oci`
  depends on the answer. Reversible because the default will live in the
  CLI or `rnk-builder` (task 11); adding it changes no `rnk-oci` type.
  `rnk_builder::image_format` (branch `feat/rnk-builder-outputs`) takes
  (a), Docker v2s2 for `FROM scratch` as go-containerregistry's empty
  image, and OCI whenever layers are zstd. Reversible: one function.
- **Raised:** 2026-10-30

### Q-007: Which image-assembly types live in rnk-oci?

- **Blocks:** Phase 1 task 1; **Area:** `rnk-oci`, `rnk-builder`
- **Context:** `ConfigEdit` (`ENV`, `LABEL`, `USER`… and history) and the
  docker-save `manifest.json` and OCI layout `index.json`/`oci-layout` types
  sit "here or in rnk-builder"
  ([rnk-oci](agents/crates/rnk-oci.md#api)); the card's Phase 1
  order already lists config edits and tar and layout index types.
- **Options:** (a) `rnk-oci`: pure types, no I/O, tested at T0;
  (b) `rnk-builder`, next to assembly and output.
- **Recommendation:** none
- **Interim:** (a), on branch `feat/rnk-oci-finish`: `ConfigEdit`,
  `ImageConfig::push_layer`/`push_history`, `DockerTarImage` and
  `OciLayout` are pure types in `rnk-oci` (`edit.rs`, `layout.rs`), marked
  `[open: Q-007]` in `crates/rnk-oci/src/lib.rs`; `rnk-builder` writes the
  files. Reversible: moving them changes import paths only.
- **Raised:** 2026-10-30

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

### Q-009: Does the parser accept instructions deferred to Phase 5?

- **Blocks:** Phase 1 task 2; **Area:** `rnk-dockerfile`
- **Context:** `ONBUILD` execution, `--mount=type=bind`,
  `COPY --link/--parents/--exclude` and `ADD <url>`/git are Phase 5, but
  kaniko's 200 Dockerfiles are Phase 1 goldens
  ([rnk-dockerfile](agents/crates/rnk-dockerfile.md#phase-1)).
- **Options:** (a) parse them so all 200 goldens pass, and let
  `rnk-builder` reject them; (b) reject them in the parser until Phase 5.
- **Recommendation:** (a), as the crate card proposes.
- **Interim:** followed the recommendation in `feat/rnk-dockerfile-parser`:
  `rnk-dockerfile` parses every instruction and flag BuildKit v0.34.0
  parses, and all 200 goldens pass. Reversible because rejecting them later
  is a check in the builder (or in `rnk-dockerfile`) on parsed values; no
  type or format depends on the choice.
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

### Q-013: How does the layer writer learn missing parent metadata?

- **Blocks:** Phase 1 tasks 5, 9; **Area:** `rnk-layer`, `rnk-snapshot`
- **Context:** the writer adds missing parent directory headers, but how it
  gets their metadata from the exporter, and how that combines with the
  `sameDirent` rule, is `[inferred]`
  ([overlay-snapshot](design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer),
  [rnk-layer](agents/crates/rnk-layer.md#phase-1)).
- **Options:** (a) the exporter hands the writer a metadata lookup;
  (b) the exporter emits every ancestor itself and the writer only sorts.
- **Recommendation:** none
- **Interim:** (a), in `feat/rnk-layer-encode`: `LayerWriter::finish` takes
  an `EntrySource` whose `parent(path)` gives a directory entry for each
  ancestor that was not fed; the writer asks once per missing ancestor,
  outermost first, and fails with `Error::MissingParent` on `None`. With the
  `sameDirent` rule the exporter skips an unchanged directory on its own and
  the writer adds it back as a parent. Reversible: (b) is the case where the
  lookup is never called.
- **Raised:** 2026-10-30

### Q-014: Are cross-repository mounts part of Phase 1?

- **Blocks:** Phase 1 tasks 6, 11; **Area:** `rnk-registry`
- **Context:** the [rnk-registry](agents/crates/rnk-registry.md#phase-1) card
  leaves them for now or later (0.04 s each in `exp-05`), while
  [phase-1.md](agents/phase-1.md#scope) lists "parallel push with
  cross-repository mount and upload fallback". A refused mount needs the
  upload fallback that `oci-client` lacks.
- **Options:** (a) Phase 1, with the upload fallback; (b) later, plain
  uploads only.
- **Recommendation:** none
- **Interim:** task 6, in `feat/rnk-registry`: the client offers both
  `push_blob` (plain upload) and `mount_or_push` (mount, and on a `202`
  refusal or any failure an upload, as kaniko); whether task 11 calls the
  mount is left to this answer. Reversible: dropping the mount is not
  calling `mount_or_push`. Task 11 (`rnk_builder::push`, branch
  `feat/rnk-builder-outputs`) takes (a): a base layer is mounted from its
  repository, and every blob from the first destination for later ones.
- **Raised:** 2026-10-30

### Q-065: Do localhost and private-network registries fall back to HTTP?

- **Blocks:** Phase 1 tasks 8, 13; **Area:** `rnk-registry`, `ranikust`
- **Context:** kaniko's go-containerregistry tries HTTPS, then plain HTTP,
  for registries named with `--insecure`/`--insecure-registry` and also,
  unasked, for `localhost:*`, loopback, `*.local` and RFC 1918 addresses
  (`kaniko@44aa7ddc0 vendor/github.com/google/go-containerregistry/pkg/name/registry.go:88-108`,
  `pkg/v1/remote/transport/ping.go:47-58`). A pull or push to
  `10.0.0.5:5000` therefore works in kaniko without a flag, but may go out
  in clear text.
- **Options:** (a) the same heuristic, for parity; (b) only registries
  the user lists fall back to HTTP.
- **Recommendation:** none
- **Interim:** (b), in `feat/rnk-registry`: `ClientOptions::insecure`
  lists the registries that may fall back; others are HTTPS only.
  Reversible: (a) adds a host test where the client decides the scheme.
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

### Q-100: How durable is a published store entry after a power loss?

- **Blocks:** nothing yet; **Area:** `rnk-store`, `rnk-snapshot`
- **Context:** an entry is published by `rename` from `tmp/`. Without a
  sync, a crash of the host (not of ranikust) can leave a renamed blob
  empty or short, or a layer missing files, and later builds would trust it
  (ADR-0006, threat T13). `syncfs` after each extraction costs time on
  large layers ([cache-and-store](design/cache-and-store.md#atomic-publication)).
- **Options:** (a) `fsync` blobs only, check nothing at start-up; (b) also
  `syncfs` the state directory after each layer; (c) record entries in a
  journal and re-verify the unjournaled ones at start-up; (d) no sync.
- **Recommendation:** none
- **Interim:** (a) on `feat/rnk-store` (`Store::fill`). Reversible: one
  call in `fill`.
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
  caps, and the overlay part is a placeholder until task 9.
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
  `sandbox/` subdirectory, and the overlay probe of task 9 gets the same
  path. Revisit with kaniko mode (Phase 2).
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

### Q-022: What format does the secret flag take?

- **Blocks:** Phase 1 tasks 8, 11; **Area:** `ranikust`, `rnk-sandbox`
- **Context:** `RUN --mount=type=secret` is Phase 1, so `--secret` is
  needed ([kaniko-map](agents/kaniko-map.md#flags)). kaniko parses
  `id=…,type=file|env,src=…,env=…`
  (`kaniko@44aa7ddc0 pkg/config/options.go:271`).
- **Options:** (a) kaniko's syntax, file and env sources; (b) a subset in
  Phase 1, the rest later.
- **Recommendation:** none
- **Interim:** `feat/ranikust-cli` refuses `--secret` with a message
  naming this question. Reversible: accepting it is one more flag, and no
  build runs yet.
- **Raised:** 2026-10-30

### Q-023: Which other kaniko flags and variables are accepted?

- **Blocks:** Phase 1 tasks 8, 13; **Area:** `ranikust`
- **Context:** about 30 kaniko flags are still to be decided with the list
  ([kaniko-map](agents/kaniko-map.md#flags): `--verbosity`, `--log-format`,
  `--label`, `--image-format`, `--digest-file`, `--insecure*`,
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

### Q-081: Does the context sub-path flag apply to a local directory?

- **Blocks:** Phase 1 task 8, if the flag is accepted (Q-023); **Area:**
  `ranikust`, `rnk-context`
- **Context:** kaniko joins `--context-sub-path` to a fetched context only:
  a `--context` without `://` returns before that line, so the flag is
  silently ignored for a bare path, while `dir://` reaches it
  (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:599-601`, `:625-630`;
  [rnk-context](agents/crates/rnk-context.md#pitfalls)).
- **Options:** (a) as kaniko: ignore it for a bare path; (b) apply it to
  every context; (c) reject a sub-path with a bare path, with a message.
- **Recommendation:** none
- **Interim:** `rnk-context` offers `Context::sub_context` for any context
  (`feat/rnk-context`), confined to the context with `..` clamped; the CLI
  decides when to call it. Reversible: no caller yet.
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

### Q-111: Does the store open before or after the probes?

- **Blocks:** Phase 1 tasks 7, 8; **Area:** `ranikust`, `rnk-store`,
  `rnk-sandbox`
- **Context:** the card's order runs the probes, then opens the store
  (marker check, start-up clean-up), then starts the spawner. But the
  sandbox probe already creates `<state-dir>/sandbox/runs` and replays its
  journals, so a state directory gets written before its `.ranikust-root`
  marker is checked, and a non-empty directory without a marker would be
  refused only afterwards ([ranikust](agents/crates/ranikust.md#api),
  [cache-and-store](design/cache-and-store.md#store-layout)).
- **Options:** (a) open the store first (in overlay mode the state
  directory is known before the probes, Q-018), then probe; (b) probe
  first, in a probe-only directory; (c) as now, and `Store::open` accepts
  a `sandbox/` it finds without a marker.
- **Recommendation:** (a), once `rnk-store` exists.
- **Interim:** (c) until task 7: `feat/ranikust-setup` probes before any
  store exists; the seam for `Store::open` is marked in
  `crates/ranikust/src/setup.rs`, after the outcome and before
  `Sandbox::start`. As a stand-in for the marker and the workdir checks
  (Q-011), the set-up creates a missing state directory with mode 0700 and
  refuses one, or any directory above it up to the workdir, that is a
  symlink, not owned by the effective uid, or writable by group or others,
  before the probe writes there (`prepare_state_dir`). The sandbox does not
  trust the directory either: journals and the run directories are opened
  without following symlinks, a journal's root must lie below the state
  directory, and a symlinked `sandbox/` or `sandbox/runs` is refused.
  Reversible: one function in `setup.rs`, replaced by the store's checks.
- **Raised:** 2026-10-30

### Q-077: How is the MS_MOVE fallback kept from a chroot escape?

- **Blocks:** Phase 3 (the fallbacks); **Area:** `rnk-sandbox`
- **Context:** where seccomp blocks `pivot_root` (Docker's default profile),
  the child moves the stage root onto `/` and `chroot`s into it, as runc's
  `--no-pivot`. The old root stays mounted underneath, and root in `RUN`
  keeps `CAP_SYS_CHROOT` (a Docker default), so the classic escape is
  `chroot` into a subdirectory, then `chdir("..")` up. The kernel's `..`
  (`follow_dotdot`, `choose_mountpoint` in `fs/namei.c`) does not stop on a
  mount point that is itself a mount root, so when `/` is a mount root the
  walk stays at the moved root; when `/` is a plain directory (ranikust
  inside a `chroot`), it climbs into ranikust's filesystem, with the state
  directory and registry credentials [inferred]
  ([sandbox](design/sandbox.md#variants-by-environment)). Measured with
  `just test-vm smoke` on kernels 5.15 and 6.8
  ([testing](testing.md#the-vm-harness-tier-t2)): under Docker's seccomp
  with `CAP_SYS_ADMIN` the probe picks `MS_MOVE`, and the `..` escape,
  its `O_PATH` + `fchdir` and nested-chroot forms, and `/proc/1/root` and
  `/proc/self/root/..` all stay inside the stage root, as under
  `pivot_root`; the same three escapes leave a plain `chroot` of that root.
  ranikust run inside a plain `chroot` refuses before any `RUN`, already
  at making `/` a slave mount (`EINVAL`), with an error that does not say
  why.
- **Options:** (a) drop `CAP_SYS_CHROOT` from the bounding set under
  `MS_MOVE` (a `RUN` that runs `chroot` fails); (b) keep Docker's defaults,
  and use `MS_MOVE` only where `/` is a mount root; (c) refuse the full
  sandbox under `MS_MOVE` and run `Isolation::Direct`, kaniko's level of
  isolation.
- **Recommendation:** (b): the VM tests above confirm that `..` stops at
  the moved root.
- **Interim:** (b): the probe falls back to `MS_MOVE` only when
  `pivot_root` fails with `EPERM` and `statx` reports `/` as a mount root;
  both root entries keep the 14 defaults (`crates/rnk-sandbox/src/probe.rs`,
  `child.rs`, `feat/rnk-sandbox`). Reversible: one bit in the drop list.
- **Raised:** 2026-10-30

### Q-024: What is the Snapshotter signature?

- **Blocks:** Phase 1 task 9; **Area:** `rnk-snapshot`
- **Context:** `ADR-0002` names the trait; its signature is designed in
  task 9 ([rnk-snapshot](agents/crates/rnk-snapshot.md#api-planned),
  [glossary](agents/glossary.md#terms),
  [ARCHITECTURE.md](../ARCHITECTURE.md#rnk-snapshot)).
- **Options:** the card's sketch (`prepare(base, parent)`, `final_root`),
  or the shape task 9 arrives at.
- **Recommendation:** none
- **Interim:** the card's sketch.
- **Raised:** 2026-10-30

### Q-025: Are other user xattrs kept in exported layers?

- **Blocks:** Phase 1 task 9; **Area:** `rnk-snapshot`, `rnk-layer`
- **Context:** the exporter strips `*.overlay.*` and keeps
  `security.capability` and other xattrs, so digests differ from BuildKit,
  which strips other `user.*` xattrs
  ([overlay-snapshot](design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer),
  [open questions](design/overlay-snapshot.md#open-questions)). kaniko writes
  only `security.capability` ([rnk-layer](agents/crates/rnk-layer.md#refs)).
- **Options:** (a) keep them; (b) strip them, as BuildKit does.
- **Recommendation:** none
- **Interim:** kept, as the exporter table says today.
- **Raised:** 2026-10-30

### Q-026: When and how are layers merged near the 500-lower limit?

- **Blocks:** Phase 1 task 9; **Area:** `rnk-snapshot`
- **Context:** overlay takes at most 500 lower directories; the design says
  to merge the oldest diffs into one temporary layer close to the limit, but
  not when or how ([overlay-snapshot](design/overlay-snapshot.md#mounting),
  [rnk-snapshot](agents/crates/rnk-snapshot.md#phase-1)).
- **Options:** (a) merge at a fixed threshold below 500; (b) merge only
  when the next mount would exceed it.
- **Recommendation:** none
- **Interim:** none
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

### Q-130: Where kaniko and BuildKit differ on a config edit, which one wins?

- **Blocks:** Phase 1 task 11 (config goldens); **Area:** `rnk-oci`, `rnk-builder`
- **Context:** `ConfigEdit::apply` (`crates/rnk-oci/src/edit.rs`) follows
  kaniko's `ExecuteCommand`s. BuildKit differs [inferred, from its
  `dockerfile2llb` dispatchers, not checked at a pinned commit]: (1) a new
  `ENTRYPOINT` resets a `CMD` inherited from the base unless the stage set
  one, as the Dockerfile reference documents; kaniko keeps it
  (`kaniko@44aa7ddc0 pkg/commands/entrypoint.go`); (2) `EXPOSE` accepts
  `sctp`, lower-cases the protocol and expands ranges (`nat.ParsePortSpecs`);
  kaniko accepts only `tcp`/`udp` and keeps `8000-8001/tcp` as one key
  (`pkg/commands/expose.go:47-62`); (3) `HEALTHCHECK --start-interval` is
  kept; kaniko drops it (go-containerregistry's `HealthConfig` has no such
  field, `pkg/commands/healthcheck.go:26-34`); (4) an absolute `WORKDIR`
  is cleaned (`/a/../b/` becomes `/b`) [inferred]; kaniko keeps it as
  written (`pkg/commands/workdir.go:39-51`).
- **Options:** (a) kaniko for each, for users migrating from kaniko;
  (b) BuildKit for each, the documented Dockerfile behaviour (as Q-051 and
  `ADR-0027` chose for the front end); (c) per item.
- **Recommendation:** none; (1) changes what a container runs, so it
  deserves a decision before the config goldens.
- **Interim:** (a) for (1), (2) and (4); `HealthConfig::start_interval` is kept
  when the builder sets it, so (3) is the builder's choice. Reversible: (1)
  needs a per-stage "CMD set" bit in `rnk-builder`, (2) a port parser,
  (4) one call to the cleaning `WORKDIR` already does for relative paths.
- **Raised:** 2026-10-30

### Q-131: Which instructions get a history entry, with which fields?

- **Blocks:** Phase 1 task 11; **Area:** `rnk-builder`
- **Context:** kaniko appends a history entry only with a layer, `author:
  "kaniko"`, `created_by` the instruction's text
  (`kaniko@44aa7ddc0 pkg/executor/build.go:1398-1405`,
  `pkg/constants/constants.go:25`); BuildKit writes one per instruction,
  `empty_layer: true` for metadata ones, with a `buildkit.dockerfile.v0`
  comment [inferred]. `rnk-oci` offers both through
  `ImageConfig::push_layer` and `push_history`.
- **Options:** (a) kaniko's; (b) BuildKit's; (c) BuildKit's entries with
  an author `ranikust`.
- **Recommendation:** none
- **Interim:** a variant of (a), on branch `feat/rnk-builder-outputs`:
  `rnk_builder::layer_history` gives kaniko's entry for a layer, with the
  author `ranikust` (`HISTORY_AUTHOR`); metadata instructions get none.
  Reversible: one function and one constant, and the executors that call
  them are not written yet.
- **Raised:** 2026-10-30

### Q-160: Are base layers rewritten under `--reproducible`?

- **Blocks:** Phase 1 task 13 (digests compared with kaniko);
  **Area:** `rnk-builder`
- **Context:** kaniko's `--reproducible` runs `mutate.Canonical` over the
  whole image, which re-encodes every layer, the base image's included,
  with zero timestamps (`kaniko@44aa7ddc0 pkg/executor/build.go:2104-2127`,
  `vendor/github.com/google/go-containerregistry/pkg/v1/mutate/mutate.go:674`),
  unless `FF_KANIKO_REPRODUCIBLE_PRESERVE_BASE_LAYERS`. Base layers are
  already content-addressed, so keeping them does not make two builds
  differ; rewriting them costs a decode and an encode of every base layer
  and a push of all of them.
- **Options:** (a) keep base layers as pulled (kaniko's feature flag);
  (b) rewrite them, as kaniko by default.
- **Recommendation:** none
- **Interim:** (a), on branch `feat/rnk-builder-outputs`: `assemble` keeps
  base `BlobLayer`s; only built layers get the fixed mtime. Reversible: (b)
  is a `LayerSource` over a pulled blob, exported like a built layer.
  Because a kept base layer keeps its compression, a zstd base layer has no
  Docker media type: `assemble` then writes OCI although `--reproducible`
  asks for Docker v2s2 (Q-006). Under (b) the layer would be re-encoded
  and Docker would hold.
- **Raised:** 2026-10-30

### Q-161: Which fixed time does `--reproducible` write?

- **Blocks:** Phase 1 task 13; **Area:** `rnk-builder`
- **Context:** kaniko's `mutate.Canonical` sets the config's and every
  history entry's `created` to Go's zero time, written
  `0001-01-01T00:00:00Z` (`mutate.go:533-536`), and tar mtimes to that
  time too. ranikust's rule is "a fixed epoch, or `SOURCE_DATE_EPOCH` when
  set" (`docs/agents/rules.md`), which kaniko never reads.
- **Options:** (a) the Unix epoch, `1970-01-01T00:00:00Z`, the same as
  `SOURCE_DATE_EPOCH=0` and as mtime 0 in a tar header; (b) Go's zero time,
  as kaniko writes.
- **Recommendation:** none
- **Interim:** (a), on branch `feat/rnk-builder-outputs`
  (`BuildTime::reproducible`). Reversible: one constant.
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

### Q-132: Are empty values of a base config written back?

- **Blocks:** Phase 1 task 13 (config comparison); **Area:** `rnk-oci`
- **Context:** `ImageConfig` writes back any present value, even `"User":
  ""` or `"Tty": false` from a base built by the classic Docker builder.
  kaniko re-serializes the base through go-containerregistry, whose fields
  are `omitempty` (`kaniko@44aa7ddc0
  vendor/github.com/google/go-containerregistry/pkg/v1/config.go:31-140`),
  so such fields disappear and the config digest differs from ranikust's.
  `ConfigEdit` already leaves out an empty `Cmd`/`Entrypoint` and a false
  `ArgsEscaped`.
- **Options:** (a) keep present values (today); (b) drop empty strings,
  `false`, empty lists and maps on writing, as Go's `omitempty`.
- **Recommendation:** none
- **Interim:** (a).
- **Raised:** 2026-10-30

### Q-120: Where kaniko and BuildKit copy differently, which does `copy` follow?

- **Blocks:** Phase 1 task 11; **Area:** `rnk-rootfs`, `rnk-builder`
- **Context:** `rnk_rootfs::copy` copies one resolved `COPY`/`ADD`
  source ([rootfs](design/rootfs.md#copy)). Where the two differ: (1) a
  symlinked source: kaniko copies the link
  (`kaniko@44aa7ddc0 pkg/commands/copy.go:150`), BuildKit follows it
  [inferred]; (2) xattrs: kaniko copies only `security.capability`
  (`pkg/util/fs_util.go:1533`), BuildKit all of them [inferred];
  (3) an existing destination directory: kaniko chowns it and sets its
  times (`pkg/util/fs_util.go:871-888`), BuildKit leaves it [inferred];
  (4) special files: kaniko reads device nodes and sockets as files unless
  `FF_KANIKO_COPY_SKIP_SPECIAL_FILES` (`pkg/commands/copy.go:170`) and
  recreates FIFOs; (5) `--chmod` on directories: kaniko applies it to every
  copied directory (`pkg/util/fs_util.go:898`); (6) symbolic `--chmod`
  (`u+x`): kaniko accepts it (`pkg/util/command_util.go:447`).
- **Options:** per item, (a) kaniko or (b) BuildKit, settled by a golden
  against both (`kaniko-parity`).
- **Recommendation:** none
- **Interim:** on branch `feat/rnk-rootfs-copy`: (1) BuildKit, followed
  inside the source root; (2) kaniko, which also keeps host labels such as
  `security.selinux` out of layers; (3) BuildKit, left as it is; (4)
  sockets skipped, FIFOs and device nodes recreated, never opened; (5)
  kaniko; (6) octal only (`CopyOpts::chmod: Option<u32>`). Reversible:
  each is one branch in `crates/rnk-rootfs/src/copy.rs` and its tests;
  nothing calls `copy` yet.
- **Raised:** 2026-10-30

### Q-121: Which overlay forms does the lower-layer lookup recognise?

- **Blocks:** Phase 1 task 9; **Area:** `rnk-rootfs`, `rnk-snapshot`
- **Context:** `extract_overlay` finds a hardlink target in the lower
  layers as overlay's merged view would
  ([rootfs](design/rootfs.md#extraction)). It recognises the form
  ranikust's own extraction writes for a `userxattr` mount (`ADR-0016`):
  whiteouts as the device 0/0, opaque directories as
  `user.overlay.opaque=y`. Overlay also knows `trusted.overlay.opaque`
  (mounts without `userxattr`), the value `x` (Linux 6.7: a directory
  holding "xwhiteouts", regular files marked `overlay.whiteout`, which is
  not opaque) and redirects. A `layers/<id>` written by anything else, or
  a later switch to `trusted.*`, would be misread.
- **Options:** (a) only ranikust's own form, `layers/` being written by
  ranikust alone; (b) also `trusted.overlay.opaque=y`; (c) the full set,
  xwhiteouts and redirects included.
- **Recommendation:** (a) while `rnk-store` holds the only writers.
- **Interim:** (a), on branch `feat/rnk-rootfs-copy`; overlay's own
  xattrs (`trusted.overlay.*`, `user.overlay.*` but the escaped
  `user.overlay.overlay.*`) are never copied up. Reversible: one function,
  `walk_layer` in `crates/rnk-rootfs/src/extract.rs`.
- **Raised:** 2026-10-30

### Q-080: Who resolves COPY sources against the context, and how?

- **Blocks:** Phase 1 task 11; **Area:** `rnk-builder`, `rnk-context`
- **Context:** `rnk-context` must not decide which files an instruction
  copies ([rnk-context](agents/crates/rnk-context.md#must-not)), but
  something must expand `COPY` source globs and keep ignored files out.
  kaniko lists the whole context and matches each source with Go's
  `filepath.Match` (`kaniko@44aa7ddc0 pkg/util/command_util.go:145-185`),
  then drops excluded files; BuildKit only ever sees the files its
  filtered walk transferred and resolves wildcards in its copy backend.
  `Context::root()` sees ignored files too.
- **Options:** (a) `rnk-builder` resolves sources against the walk's
  entries (or `root()` plus the same `Matcher`), with BuildKit's wildcard
  rules; (b) `rnk-context` gains a resolver, e.g.
  `resolve_sources(&[pattern], &Matcher) -> Vec<Entry>`, and the builder
  only copies; which glob dialect (`filepath.Match` as kaniko, or
  BuildKit's) is part of either.
- **Recommendation:** (a), which the card's boundary already implies;
  the dialect needs a golden against BuildKit.
- **Interim:** `rnk-context` exposes the filtered walk and `root()` only
  (`feat/rnk-context`); nothing resolves globs yet. Reversible: adding a
  resolver to either crate later is additive.
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

### Q-043: What are the prune defaults?

- **Blocks:** Phase 5; **Area:** `rnk-store`
- **Context:** `prune` removes entries by total size and age under the
  build locks; the defaults, the age measure, in-use tracking and size
  accounting for reflinked copies are open
  ([cache-and-store](design/cache-and-store.md#prune),
  [open questions](design/cache-and-store.md#open-questions),
  [rnk-store](agents/crates/rnk-store.md#api-planned)).
- **Options:** age by (a) last use or (b) creation; defaults: none listed.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-044: How do remote ONBUILD triggers reach the stage graph?

- **Blocks:** Phase 5; **Area:** `rnk-dockerfile`
- **Context:** `rnk-dockerfile` does no I/O and takes remote `ONBUILD`
  triggers from the caller, while kaniko fetches them during planning
  (`kaniko@44aa7ddc0 pkg/dockerfile/dockerfile.go:354`;
  [rnk-dockerfile](agents/crates/rnk-dockerfile.md#api-planned)).
- **Options:** (a) an extra input to `stage_graph()`, filled after base
  configs are pulled; (b) a separate step that extends the graph.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-045: How are git contexts fetched and addressed?

- **Blocks:** Phase 5; **Area:** `rnk-context`
- **Context:** a static binary in `scratch` has no `git` to call; kaniko's
  `git://` reads `#ref#commit`, Docker uses `#ref:dir`; the git tests depend
  on the choice ([rnk-context](agents/crates/rnk-context.md#pitfalls),
  [tests](agents/crates/rnk-context.md#tests)).
- **Options:** (a) a Rust git implementation or (b) an external `git`;
  fragments (c) kaniko's, (d) Docker's, or both.
- **Recommendation:** none
- **Interim:** none
- **Raised:** 2026-10-30

### Q-046: What happens to HTTPS contexts on Azure Blob hosts?

- **Blocks:** Phase 5; **Area:** `rnk-context`
- **Context:** kaniko sends `https://` contexts on Azure Blob hosts to an
  Azure source (`kaniko@44aa7ddc0 pkg/buildcontext/buildcontext.go:63-65`),
  which ranikust does not have (`ADR-0008`;
  [rnk-context](agents/crates/rnk-context.md#pitfalls)).
- **Options:** (a) reject them with a message; (b) fetch them as plain
  HTTPS.
- **Recommendation:** none
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

### Q-049: Which overlay optimizations come after Phase 1?

- **Blocks:** nothing yet; **Area:** `rnk-snapshot`
- **Context:** keeping one mount for consecutive instructions (its gain
  against the `volatile` remount rules), and an export-time filter dropping
  copied-up files identical to their lower version (off by default; BuildKit
  does not filter)
  ([overlay-snapshot](design/overlay-snapshot.md#the-volatile-lifecycle),
  [open questions](design/overlay-snapshot.md#open-questions),
  [rnk-snapshot](agents/crates/rnk-snapshot.md#phase-1)).
- **Options:** for each, (a) build it later; (b) drop it.
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
