# rnk-builder

> **Status:** in progress · **Phase:** 1-2 (cache checks: 4) ·
> **Depends on:** `rnk-snapshot`, `rnk-sandbox`, `rnk-layer`, `rnk-registry`,
> `rnk-context`, `rnk-ignore`, `rnk-dockerfile`, `rnk-store`, `rnk-rootfs`,
> `rnk-oci` (`rnk-cache` planned) ·
> **Used by:** `ranikust` (its `rnk-sched` runner) ·
> **Path:** `crates/rnk-builder/`

**TL;DR:** Builds one stage: one executor per instruction, diffs through
`rnk-snapshot`, `RUN` through `rnk-sandbox`, encoding only the layers that are
needed (lazy export), image config updates, then image assembly and output.
Keep it free of global state (paths come as `BuildPaths`) and its output
independent of timing.

## Owns

- One executor per instruction and the `RUN --mount` and secret handling
  (`stage.rs`, `mounts.rs`, `sources.rs`); cache checks come in Phase 4.
- Lazy layer export: a layer is encoded only when it is pushed, cached or
  part of the target.
- Image config updates, image assembly and output: registry push, Docker tar,
  OCI layout.

## Must not

Boundaries: `crates/rnk-builder/src/lib.rs`. In short:

- Keep global state (no process-wide cwd, `PATH` or environment changes):
  paths arrive as `rnk-store`'s `BuildPaths`; the clock and
  `SOURCE_DATE_EPOCH` arrive as values.
- Order stages (`rnk-sched`; this crate does not depend on it, `ranikust`
  implements its `StageRunner`), parse flags (`ranikust`) or the Dockerfile
  (`rnk-dockerfile`); mount or diff (`rnk-snapshot`), fork `RUN`
  (`rnk-sandbox`), compute keys (`rnk-cache`), write tar (`rnk-layer`).
- Touch a path in a root except through `RootPath`; reach an output except
  through `rnk_rootfs::OutputDir` (ADR-0076).

## API

### Output side

```rust
pub struct BuildTime;                                   // one clock reading per build
impl BuildTime {
    pub fn at(now: SystemTime) -> Self;                 // normal build: config `created`, history
    pub fn reproducible(source_date_epoch: Option<&str>) -> Result<Self>;  // epoch or SOURCE_DATE_EPOCH (ADR-0032)
    pub fn layer_mtime(&self) -> Option<i64>;           // for EncodeOptions::mtime: later mtimes clamped to it
    pub fn rfc3339(&self) -> String;                    // Go's RFC3339Nano, as kaniko writes
}
pub struct LayerDemand { pub push: bool, pub cache: bool, pub target: bool, pub output: bool, pub saved: bool }
impl LayerDemand {
    pub fn for_stage(in_target: bool, outputs: &Outputs, cache: bool) -> Self;  // [open: #8] the rule lives here; ranikust (build.rs) calls it
    pub fn saved(self, saved: bool) -> Self;  // kaniko mode: a later stage is built FROM it; encoded, never pushed or written
    pub fn needed(&self) -> bool;
}
pub trait LayerSource: Send {                           // rnk-snapshot's exporter implements it
    fn encode(self: Box<Self>, options: EncodeOptions, out: &mut dyn Write) -> Result<LayerDigests, rnk_layer::Error>;
}
pub struct EntriesLayer<S: EntrySource + Send>;         // entries plus a content source
pub struct BlobLayer { pub diff_id: Digest, pub blob: Descriptor, pub bytes: LayerBytes, pub mount_from: Option<Reference> }
pub enum LayerBytes { File(PathBuf), Anon(AnonBlob), Registry(Reference) }  // Registry: a base layer never kept, mounted or streamed from its source on push (ADR-0055)
impl BlobLayer { pub fn base(..) -> Self; pub fn remote(..) -> Self; pub fn path(&self) -> Option<&Path>;
                 pub fn open(&self) -> Option<io::Result<File>>; }  // a new descriptor of the kept bytes
pub struct AnonBlob;  // kaniko mode on `/`: an encoded layer in a scratch_file shared by every copy; release() frees it for
                      // every copy, after which open() fails: no /proc/self/fd/<n> name outlives it
pub struct LayerExporter;                               // new(dir = Store::build_dir(), options, &BuildTime); anonymous(store, ..): anonymous files of the store (ADR-0051)
impl LayerExporter {
    pub fn export_if_needed(&self, source: Box<dyn LayerSource>, demand: LayerDemand) -> Result<Option<BlobLayer>>;
    pub fn export(&self, source: Box<dyn LayerSource>) -> Result<BlobLayer>;
    pub fn export_unless_empty(&self, source: Box<dyn LayerSource>) -> Result<Option<BlobLayer>>;  // None for a layer without entries (ADR-0058); EMPTY_LAYER_DIFF_ID
    pub fn with_uploader(self, uploader: Uploader) -> Self;  // ADR-0055: pushed layers go to the destinations as exported
    pub fn exported(&self, layer: &BlobLayer, demand: LayerDemand) -> Result<()>;  // hands a pushed layer to the uploader; Err once an upload failed: the stage stops at its next layer
    pub async fn finish_uploads(&self) -> Result<BTreeSet<Digest>>;  // the digests every destination holds
    pub async fn cancel_uploads(&self);                 // after a failed build: drops running uploads, releases the files
}
pub struct Uploader;  // Uploader::start(client, destinations, jobs, Release::{Keep, Free(store)}, &Handle): its own thread driving the runtime;
                      // at most `jobs` layers at once and `jobs` waiting (submit blocks: stage threads only); a layer is released once every destination has it, or on failure
pub const HISTORY_COMMENT: &str = "buildkit.dockerfile.v0";
pub fn instruction_history(created_by: &str, time: &BuildTime) -> History;  // BuildKit's entry, no author
pub fn image_format() -> ImageFormat;                   // OCI, as BuildKit (ADR-0032)
pub struct Image { pub format: ImageFormat, pub config: Bytes, pub config_descriptor: Descriptor, pub manifest: RawManifest, pub layers: Vec<BlobLayer> }
pub fn assemble(config: ImageConfig, layers: Vec<BlobLayer>, format: ImageFormat, time: &BuildTime) -> Result<Image>;  // assemble_annotated adds annotations
pub struct Outputs { pub destinations: Vec<Reference>, pub push: bool, pub docker_tar: Option<PathBuf>, pub oci_layout: Option<PathBuf> }
impl Outputs {
    pub fn new(destinations: &[String], push: bool, docker_tar: Option<PathBuf>, oci_layout: Option<PathBuf>) -> Result<Self>;
    pub fn tar_tags(&self) -> &[String];                // as typed, `:latest` added; UNSET_DESTINATION without one
}
pub fn write_oci_layout(image: &Image, dir: &Path, fence: Option<&Fence>) -> Result<()>;  // through rnk_rootfs::OutputDir, by its rules: no symlink followed, never into `fence` (the build context), files replaced as its writes say (ADR-0076)
pub fn write_docker_tar(image: &Image, tags: &[String], path: &Path, fence: Option<&Fence>) -> Result<()>;  // the same
pub async fn push(image: &Image, destinations: &[Reference], client: &Arc<Client>, jobs: NonZeroUsize) -> Result<Vec<(Reference, Digest)>>;
```

The caller writes the outputs in kaniko's order: OCI layout, Docker tar, push.
`export` and the two file outputs block: call them from a blocking thread
when a runtime runs. Every file is written to a randomly named, exclusively
created temporary file and renamed into place, so a planted name is never
followed and an error leaves no partial output. `push` fails with
`Error::Push`, naming the destination that failed and those already pushed;
the first failed blob cancels the other transfers to that destination, and
later destinations are not tried.

### Stage executor

Blocking: call it on a thread of its own.

```rust
pub fn build_stage<S: Snapshotter>(cx: &BuildCx<'_, S>, input: StageInput<'_, S::Root, S::Layer>) -> Result<StageDone<S::Root>>;
pub struct BuildCx<'a, S> { pub snapshotter: &'a S, pub sandbox: &'a Sandbox,
    pub isolation: Isolation,                            // Full, or Direct on kaniko mode's shared root (ADR-0043)
    pub store: &'a Store, pub context: &'a Context, pub context_files: &'a ContextFiles, pub exporter: &'a LayerExporter,
    pub time: &'a BuildTime, pub network: Network, pub secrets: &'a BTreeMap<String, SecretValue>,
    pub allow_network_host: bool,                        // --allow network.host
    pub platform: &'a Platform, pub run_gate: &'a dyn RunGate, pub cancelled: &'a (dyn Fn() -> bool + Sync) }
pub enum Secret { File(PathBuf), Env(OsString) }         // a --secret's source
impl Secret { pub fn load(&self, id: &str) -> Result<SecretValue>; }  // read once at the start, never by path again (ADR-0078): a regular file of at most MAX_SECRET_SIZE (500 KiB, as BuildKit), opened without blocking; a variable, unset = empty
pub struct SecretValue { /* Arc<[u8]> */ }               // shared by every RUN that mounts it; Debug hides it; zeroed when the last holder drops it ([open: #26] for a RUN's copies)
pub trait RunGate: Sync { fn enter(&self) -> Option<Box<dyn Send>>; }  // a --jobs permit per RUN; Ungated for tests
pub struct StageInput<'a, R, L> { pub index, pub graph: &StageGraph, pub stage: &Stage, pub base: StageBase<'a, R, L>,
    pub stages: BTreeMap<usize, &StageDone<R>>, pub images: BTreeMap<String, &ImageSource<R, L>>, pub demand: LayerDemand,
    pub labels: &[(String, String)], pub take_base: bool }
pub enum StageBase<'a, R, L> { Scratch, Image(&'a PulledImage<L>), Stage(&'a StageDone<R>) }
pub struct StageDone<R> { pub root: SourceRoot<R>, pub scope: StageScope, pub config: ImageConfig,
    pub layers: Vec<BlobLayer>, pub complete: bool, pub base_format: Option<ImageFormat> }
pub struct SourceRoot<R>;                                // a root behind a mutex; release() once, children first; take(): the root out, for a background release (ADR-0061)
impl SourceRoot<R> { pub fn keep(&self, need: &KeepNeed) -> Result<()>; } // kaniko: keep what later stages read
pub enum KeepNeed { All, Sources(Vec<String>) }          // COPY --from sources: their entries + links on the way, checked to resolve the same in the copy, else All
pub struct ImageSource<R, L> { pub image: Arc<PulledImage<L>>, pub root: SourceRoot<R> }  // COPY --from=<image>
pub struct ContextFiles;                                 // ContextFiles::load(&Context, Option<&Matcher>)
pub async fn pull_image<S>(client: &Arc<Client>, store: &Arc<Store>, snapshotter: &Arc<S>, reference: &str, platform: &Platform, keep_blobs: bool) -> Result<PulledImage<S::Layer>>;
    // blobs into the store, base layers by the snapshotter; unless keep_blobs (a file output), a snapshotter that
    // streams (kaniko) gets each layer as a stream from the registry and no blob is pulled (ADR-0055)
pub struct PulledImage<L = LayerDir> { pub reference, pub config: ImageConfig, pub format: ImageFormat, pub blobs: Vec<BlobLayer>, pub layers: Vec<L> }
```

- `take_base` (ADR-0061, ADR-0062): this stage is its base stage's only
  reader, from the stage graph; `build_stage` then calls
  `snapshotter.prepare_last(parent)` instead of `prepare(Stage(parent))`,
  under the parent's lock.
- `build_stage` calls `root.set_layers_needed(demand.needed())` once after
  `prepare` (ADR-0046) and `root.done()` after the last instruction, before
  the root goes into `StageDone` (ADR-0059); a `RUN`'s spec gets
  `spec.expect_root(root)` for the root the layer holds (ADR-0059).

### Instruction behaviour

- `RUN`, `COPY`, `ADD` and a `WORKDIR` other than `/` each run in a layer of
  their own (`WORKDIR` makes its directory with `mkdir -p`, owned by the
  active user). A step whose layer is empty adds no layer, only its history
  entry with `empty_layer: true` (ADR-0058, `EMPTY_LAYER_DIFF_ID`). The other
  instructions edit the config through `rnk_oci::ConfigEdit` and mount
  nothing; `VOLUME` records the config only (ADR-0032). A closed layer is
  encoded at once when the stage's `LayerDemand` needs it.
- History as BuildKit writes it (ADR-0032, ADR-0058): every instruction but
  `ONBUILD` adds an entry; `created_by` in BuildKit's text (`history.rs`:
  Go's `%q`/`%v`/`%+v`, `RUN` prefixed with the stage's `ARG`s as `|N k=v`,
  ` # buildkit` after `RUN`, `COPY`, `ADD`), `empty_layer` for the others and
  for an empty step, no author, comment `buildkit.dockerfile.v0`. A non-Windows
  config without `PATH` gets the default one (ADR-0032).
- Under `--reproducible` the config's `created` is the epoch, the build's
  history entries carry it (the base's keep theirs), layer mtimes later than
  it are clamped, and base layers are kept as pulled (ADR-0032). Media types
  are OCI whatever the base and `--reproducible`; a layer without a Docker
  type still makes an image asked for as Docker OCI (ADR-0032).
- Sources resolve here, against the context walk (`ContextFiles`) or a
  stage's root, with BuildKit's rules (ADR-0032): a wildcard matching nothing
  copies nothing (its base directory must exist); several sources need no
  destination ending in `/`; a symlinked source is followed inside its root
  (in a context, only through kept entries); a copy onto a symlink to a
  directory goes inside it; `--chmod` takes octal or symbolic modes
  (`rnk_rootfs::Chmod`). With `Isolation::Direct` (no `nodev` on the root)
  `COPY` leaves device nodes, FIFOs and sockets out (`CopyOpts::skip_special`,
  ADR-0060).
- `RUN --mount` (`mounts.rs`), as BuildKit (ADR-0032): a cache mount is
  leased from the store with its `sharing=` (`Store::cache_lease`: shared;
  locked waits; private gets a new empty cache); a `RUN` takes its leases in
  key order and a key mounted twice shares one lease; the id defaults to the
  cleaned target, `uid`/`gid`/`mode` key the cache and are set through a
  descriptor of its directory. A secret's id is `id=`, else `source=`, else
  the target's name; with `env=` it is a variable of that `RUN` only (a NUL
  byte is refused, naming the id), otherwise a `rnk_sandbox::SecretFile`
  (default `0400`, root's; permission bits only) that the sandbox writes on
  the `RUN`'s own tmpfs and binds read-only at the target or
  `/run/secrets/<id>`: no copy reaches a disk. A cancel kills the running
  `RUN` (`Sandbox::cancel`).
- A `#!` `RUN` heredoc runs as a script `/dev/pipes/<name>`, as BuildKit's:
  its directory (in the build directory, mode 0755) is bound read-only at
  `/dev/pipes`, which `rnk-sandbox` makes in the `/dev` tmpfs. Under
  `Isolation::Direct` (kaniko mode on `/`) the body goes to
  `RunSpec::script` instead (sealed memfd; the argv kept only for the
  history). A cache mount is then left out with a warning (the `RUN`'s writes
  there land in its layer), and a secret as a file (no `env=`, or a `target=`)
  and `--network=none` are refused before any secret is read
  (`mounts::direct_mounts`, `refuse_direct_network`, ADR-0057). ADR-0084 lifts
  the refusal of the secret file and ADR-0085 has it placed at its target and
  removed after the `RUN`: decided, not built
  ([#35](https://github.com/tran4774/ranikust/issues/35)).
  `mounts::check_supported` refuses, in every mode, the mount types this
  phase lacks and malformed mounts, and `run_mounts` a `required` secret
  missing from `--secret`, before any secret is read or cache leased.
- Refused with a message, not built yet (Phase 5): `ONBUILD` triggers, `ADD`
  of a URL, a repository or a local archive, `ADD --checksum/--keep-git-dir/
  --unpack`, `COPY --link/--parents/--exclude`, `RUN --mount` other than
  `cache` and `secret` (and `cache,from=`). Refused for good: `RUN
  --security=insecure` and `--device`; a heredoc name that is not one path
  component (`check_heredoc_name`). An entitlement rule, not a missing
  feature: `RUN --network=host` under the operator's `--network=none`
  without `--allow network.host` (BuildKit's, ADR-0032).

## Invariants

- Instructions run in order inside a stage; metadata-only instructions mount
  nothing and add no layer.
- `COPY`/`ADD` write through the stage root (`rootfs/` in overlay mode), so
  destination symlinks resolve as in kaniko; this crate decides which context
  files they read, with `rnk-ignore`; `--chown` names resolve through
  `rnk-rootfs` in the stage root.
- A layer is encoded only if pushed, cached, written as a tar or OCI layout,
  or in the `--target` stage; then it is exported as soon as it is complete,
  with a cap on compression threads.
- Same inputs, same bytes: layer order is instruction order whatever order
  exports finish in; timestamps are fixed under `--reproducible`; nothing
  depends on `--jobs`, thread count or the host's users.
- A failed `RUN` fails the stage with its exit status and is never retried
  (it may have written part of its output). The `EXDEV` hint towards
  `--mode=kaniko` is not built: the failing program reports `EXDEV` itself,
  which the builder cannot see.
- A step that changes nothing adds no layer, only history (ADR-0058; test in
  `tests/keep.rs`). Stage squashing exists only in sequential kaniko mode.
- Built layer blobs live in the build directory, not in the store's `blobs/`,
  until Phase 4 publishes them (ADR-0052).

## Refs

- **Design:** [`design/scheduler.md#lazy-export`](../../design/scheduler.md#lazy-export), [`design/overlay-snapshot.md#one-layer-per-instruction`](../../design/overlay-snapshot.md#one-layer-per-instruction), [`#lazy-export`](../../design/overlay-snapshot.md#lazy-export), [`design/kaniko-mode.md#sequential-flow`](../../design/kaniko-mode.md#sequential-flow), [`#after-a-failed-run`](../../design/kaniko-mode.md#after-a-failed-run); [ARCHITECTURE.md](../../../ARCHITECTURE.md) (data flow of a build)
- **Decisions:** [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (kaniko flags), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (differences between modes), [ADR-0016](../../adr/0016-overlay-mount-options.md) and [ADR-0023](../../adr/0023-mode-flag.md) (`EXDEV` hint), [ADR-0005](../../adr/0005-own-versioned-cache-key.md) (cache keys), [ADR-0032](../../adr/0032-observable-behaviour-follows-buildkit.md) (BuildKit's observable behaviour), [ADR-0098](../../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) (planned, Phase 3: a build in a user namespace refuses an image with an owner it does not map, at the first failing `chown` for what the build creates)
- **Open:** [#8](https://github.com/tran4774/ranikust/issues/8) (which crate computes `LayerDemand`), [#23](https://github.com/tran4774/ranikust/issues/23) (encode a layer straight into its upload), [#26](https://github.com/tran4774/ranikust/issues/26) (wiping secrets), [#30](https://github.com/tran4774/ranikust/issues/30) (a cache on `/` across builds). Decided, not built yet: [ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md) (cache key fields), [ADR-0114](../../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md) (lookahead, skipping cached stages), [ADR-0121](../../adr/0121-a-runs-bind-mounts-read-one-filtered-copy-of-the-context-copied-in-on-slash.md) (mounts for a `Direct` `RUN`, a placed secret file's target)
- **Evidence:** [exp-08](../../research/experiments/08-parallel-stages-lazy-export.md) (lazy export saved 1.3-6.6 s), [exp-06](../../research/experiments/06-overlay-vs-walk.md) (about 2 s to export one large layer), [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) (export variance, image check), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) (lazy saved 2.8-3.6 s in parallel kaniko mode)
- **kaniko:** `kaniko@44aa7ddc0 pkg/commands/commands.go:32` `DockerCommand` (the shape to compare the executors with); `pkg/executor/build.go:603` `stageBuilder.build` (the per-instruction loop); `:1019` `shouldTakeSnapshot`; `:1034` `saveSnapshotToImage`; `:1689` `DoBuild`
- **External:** <https://github.com/opencontainers/image-spec/blob/main/config.md>, <https://docs.docker.com/reference/dockerfile/>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers). All T0 unless
noted.

- `tests/stage.rs` (a fake snapshotter on plain directories): each metadata
  instruction's config, wildcard and directory `COPY` with `.dockerignore`,
  heredoc sources, `COPY --from`, one layer and history entry per changing
  instruction, the refusals, the spans; a source expanding to an empty string
  refused; with `Isolation::Direct`, `COPY` from the context and `COPY
  --from` a stage holding FIFOs, sockets and device nodes leaves them out
  (kept with the full sandbox), and `ADD` of one fails without blocking.
  The rules of each config change are `rnk_oci::ConfigEdit`'s
  (`crates/rnk-oci/src/edit.rs`); here, that each executor builds the right
  edit.
- `tests/outputs/` (layer bytes the same at 1, 2 and 8 threads;
  `--reproducible` scrubbing; the OCI layout and Docker tar file by file and
  through `skopeo inspect` when installed; push with mounts against
  `rnk-registry`'s fake registry; the `LayerDemand` table), `tests/spans.rs`,
  `tests/stall.rs`.
- `src/stage.rs` tests and `tests/keep.rs`: kaniko-mode keeping (`COPY
  --from` sources kept with the symlinks on their way and nothing else; a
  source that cannot be kept by its paths keeps the whole stage;
  `link_closure` through relative, absolute and `..` links and a loop), and a
  red team on the real kaniko-mode snapshotter over one shared root with a
  hostile first stage (links to the ignored state, `/`, the host or a loop,
  `..` out; a FIFO source copied without blocking; a step that changes
  nothing adds no layer, ADR-0058).
- `src/mounts/tests.rs` `red_team_direct_secret_mount_spellings` (ADR-0057):
  secret mounts in every Dockerfile spelling (mixed-case keys and types,
  `dst=`, `destination=`, a repeated `target=` or `env=`, `type=` twice,
  `required=false`) are refused as files before any read, or kept as one
  variable and never a file; cache mounts of any spelling are dropped.
- **Quick smoke case `run`** (`tests/smoke/cases/run/`, `just smoke-case`
  and the VM `quick` suite, [testing](../../testing.md#quick-smoke-cases)):
  `Dockerfile` in the full sandbox (shell, exec and heredoc forms, a `#!`
  heredoc as `/dev/pipes/<name>`, cache and secret mounts, `--network=none`,
  an unchanged `RUN`); `direct.Dockerfile` in kaniko mode on `/` (the `Direct`
  forms, the dropped-cache warning, each `Direct` refusal: a secret as a
  file, `--network=none`); `direct-attack.Dockerfile` (ADR-0057 red team:
  the sealed heredoc, descriptors, streams, and refused spellings).
- **T1/T2:** `just smoke` and `just test-vm smoke` build `tests/smoke/e2e`
  with the binary and run the image; `--reproducible` gives one digest with
  `--jobs 1` and the default, in rootless podman and as real root. W1 and W3
  end to end; flattened content against kaniko
  (`nextjs-kaniko/digest-check/`) and between modes (ADR-0012 list).
  Phase 6: kaniko's 201 integration Dockerfiles in every mode.
- To do: config goldens against kaniko and BuildKit; W1-W3; export
  overlapped with the next instruction.

## Metrics

- The instruction record `rnk_builder.instruction` (`stage`, `index`, `line`,
  `kind`, `text` as written, never expanded, `cache_hit`); `stage`, `execute`,
  `layer` (`skipped`), `upload` (`bytes`, ADR-0055), `assemble`, `output`
  ([rows](../../design/metrics.md#rnk-builder)).
- Test: with `--reproducible`, digests equal with `--metrics-file` set and
  unset.

## Pitfalls

- Exporting costs about 2 s per large layer: lazy export matters as much as
  the diff method (exp-06 finding 7, exp-08).
- A late export re-reads reclaimed pages, 1.8-6.6 s for the same layer (exp-07
  finding 7); exporting every layer in parallel was 3 s slower at 32 CPUs
  (exp-08 finding 3).
- Writing `COPY` output straight into `diff-k` skips destination symlinks; go
  through `rootfs/` (overlay-snapshot design).
- An accepted kaniko flag with a slightly different meaning is a trap; each
  needs a test of its meaning (ADR-0011).
- A layer is encoded right after its instruction, on the stage's thread: the
  next instruction waits for it (exp-08 measured overlapping as the gain; not
  done yet).
- Context files are owned by `0:0` without `--chown`: an unprivileged test
  process cannot do that, so T0 tests pass `--chown` with their own ids.
- The registry client's futures are not `Send` for every lifetime: an async
  caller that must be `Send` (`rnk-sched`'s `prepare`) drives `pull_image`
  with `block_on` on a blocking thread, and must await it to the end: an
  abandoned pull still uses the runtime as it shuts down.
- A layer's media type is fixed when the image is assembled
  (`MediaType::to_format`), not when it is encoded: a base image's Docker
  layers become OCI ones in an OCI image, and the reverse. `assemble` writes
  OCI when Docker is asked for but a layer has no Docker media type (a zstd
  base layer under `--reproducible`): `Image::format` is what was written.
- The Docker tar goes through `rnk-layer`'s writer, which sorts entries:
  layers, `manifest.json`, then the config, not go-containerregistry's order
  (loaders read the whole archive, so only the file's bytes differ).
  `RepoTags` are the destinations as typed, `:latest` added when no tag was
  given, as go-containerregistry's `tag.String()` (not the normalized
  `docker.io/library/...`).
- An OCI layout's existing blob is kept only if its bytes hash to its name.
- `push` mounts from the base repository first, then from the first
  destination. `rnk-registry`'s fake registry answers `HEAD` only for
  repositories that hold a blob; otherwise no mount is ever tried.
