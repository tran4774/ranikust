# rnk-snapshot

> **Status:** done for Phases 1 and 2 (overlay as root, kaniko mode); mounting
> in a user namespace is Phase 3 · **Phase:** 1-2 ·
> **Depends on:** `rnk-rootfs`, `rnk-layer`, `rnk-store` ·
> **Used by:** `rnk-builder`, `rnk-sched` (root sharing only), `ranikust`
> (probes) · **Path:** `crates/rnk-snapshot/`

**TL;DR:** Gives each stage a root (`StageRootfs`) and turns each change into
a `LayerDiff`: overlay mode mounts a fresh upper per instruction, kaniko mode
walks and hashes. Keep `volatile` with fresh work dirs, the lower root's
metadata on the upper root, whiteouts before hardlinks; delete via
`rnk-rootfs`.

## Owns

- The `StageRootfs` and `Snapshotter` traits and their two implementations.
- Overlay: mounts with the ADR-0016 options, one upper per instruction,
  overlay to OCI whiteout conversion, base layers extracted into the store's
  `layers/` (`rnk-layer` decoder, `rnk-rootfs::extract`), the exporter that
  feeds `rnk-layer`'s writer, and the overlay and `lowerdir+` probes.
- kaniko: the parallel walk and hash with a stat cache, `WalkDiff` and its
  exporter, path resolution, the ignore list, filesystem deletion (what and
  when), the shared root (`Kaniko`, on `/` only since ADR-0059) and the
  per-stage directories (`KanikoStages`, ADR-0059).

## Must not

Boundaries: `crates/rnk-snapshot/src/lib.rs`. In short:

- Run instructions (`rnk-sandbox`, `rnk-builder`), encode layers
  (`rnk-layer`), expose a stage root as anything but a `RootPath` (the mount
  point is the one host path), or delete by path.
- Own the store (`rnk-store`), transfer blobs (`rnk-registry`), compute cache
  keys (`rnk-cache`), or switch modes mid-build.

## API

The traits are ADR-0046; the kaniko types
([design](../../design/kaniko-mode.md#stage-roots-on-the-shared-root),
[per-stage directories](../../design/kaniko-mode.md#per-stage-directories))
and overlay's follow.

```rust
pub trait ExportLayer: Send {                      // a closed layer: LayerDiff or a kaniko diff
    fn export(self, o: EncodeOptions, out: &mut dyn Write) -> Result<(LayerDigests, DiffCounts)>;
}
pub trait StageRootfs {
    type Diff: ExportLayer + 'static;
    fn set_layers_needed(&mut self, needed: bool) {} // once after prepare; kaniko walks only when true
    fn root(&self) -> Result<&RootPath>;           // writable root of the open layer (COPY, ADD, WORKDIR)
    fn mount_point(&self) -> &Path;                // host path, only for rnk-sandbox to enter for RUN
    fn begin_layer(&mut self) -> Result<()>;       // overlay: diff-k, work-k, root metadata, mount
    fn end_layer(&mut self) -> Result<Self::Diff>; // overlay: unmount, diff-k the top lower; kaniko: walk + diff
    fn view(&mut self) -> Result<&RootPath>;       // read-only between layers (USER lookup, COPY --from)
    fn keep(&mut self, what: &Keep) -> Result<()> { Ok(()) } // kaniko: copy out what later stages read, give the root up
    fn done(&mut self) {}                          // last instruction ran; a kaniko stage drops its walker (ADR-0059)
    fn release(self) -> Result<()>;                // unmount, delete the stage directory, diffs included
}
pub enum Keep { All, Paths(Vec<PathBuf>) }         // Paths: physical (no link before the last name, no ..); the root = All
pub trait Snapshotter {
    type Root: StageRootfs;
    type Layer: Clone + Debug + Send + Sync;       // LayerDir (overlay) or KanikoLayer (a blob, or streamed)
    fn base_layer(&self, diff_id: &Digest, blob: &Path, below: &[Self::Layer]) -> Result<Self::Layer>;
    fn streamed_layer(&self, diff_id: &Digest, open: Arc<dyn LayerOpen>) -> Option<Self::Layer>; // ADR-0055: kaniko Some, overlay None
    fn prepare(&self, base: Base<'_, Self::Root, Self::Layer>) -> Result<Self::Root>;  // Scratch | Image(&[Layer]) | Stage(&Root)
    fn prepare_source(&self, layers: &[Self::Layer]) -> Result<Self::Root>;  // COPY --from=<image>; default prepare(Image)
    fn prepare_last(&self, parent: &Self::Root) -> Result<Self::Root>;     // FROM <stage> by its only reader: may take the root over (ADR-0061, ADR-0062); default prepare(Stage)
}
pub trait LayerOpen: Debug + Send + Sync { fn open(&self) -> io::Result<Box<dyn Read + Send>>; } // a base layer's compressed bytes, opened per extraction

// Overlay (src/stage.rs): Snapshotter<Root = OverlayStage, Layer = LayerDir>
impl Overlay { pub fn new(store: Arc<Store>, caps: OverlayCaps) -> Result<Self>; pub fn store(&self) -> &Arc<Store>;
    pub fn base_layer_from(&self, diff_id, blob: impl Read, below: &[LayerDir]) -> Result<LayerDir>; }
pub struct LayerDir;                               // layers/<algo>/<hex>/: diff_id(), path()
pub struct LayerDiff;                              // upper + lowers; new(upper, lowers), is_empty(), entries(), export(o, out)
pub fn probe(dir: &Path) -> Result<OverlayCaps>;   // ADR-0016 mount; lowerdir+ else mount(2); write-through check
pub struct OverlayCaps { pub api: MountApi }       // MountApi::{LowerdirPlus, Mount2}
pub const OVERLAY_OPTIONS: &str = "userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile"; // [decided] ADR-0016
pub const MAX_LOWERS: usize = 500;                 // refused beyond, no merging yet (ADR-0032)

// kaniko mode (src/kaniko/): shared root, one holder (used on `/` only since ADR-0059)
impl Kaniko { pub fn new(root: RootPath, path: PathBuf, ignore: IgnoreList, walk: WalkOptions, kept: RootPath) -> Self;
    pub fn keep_in_anonymous_files(&self, store: Arc<Store>) -> Result<()>; // on `/` (ADR-0056): kept stages are tars in the store's anonymous files
    pub fn finish(&self) -> Result<bool>; }        // end of build: empties a root off `/` a stage changed; never `/`
pub const BUILT_MARKER: &str = "ranikust/.built";  // under the ignored /ranikust (ADR-0052 D1)
// Snapshotter<Root = KanikoRoot, Layer = KanikoLayer>; KanikoDiff is its ExportLayer

// kaniko mode off `/`: a directory per stage (ADR-0059), <state>/builds/<id>/stages/<n>/rootfs
impl KanikoStages { pub fn new(store: &Store, ignore: IgnoreList, walk: WalkOptions) -> Result<Self>; // refuses an anonymous store
    pub fn stages_dir(&self) -> &Path; }
// Snapshotter<Root = KanikoStage, Layer = KanikoLayer>; KanikoStageDiff is its ExportLayer

pub struct IgnoreSpec<'a> { pub root: &'a Path, pub state_dir: Option<&'a Path>, pub ignore_paths: &'a [PathBuf] }
pub fn ignore_list(spec: &IgnoreSpec<'_>, mounts: &[MountInfo]) -> Result<rnk_rootfs::IgnoreList>;
pub fn read_mountinfo() -> Result<Vec<MountInfo>>; pub fn parse_mountinfo(text: &[u8]) -> Result<Vec<MountInfo>>; // MountInfo has device (major:minor) and fs_root for the state-dir check

pub struct Walker;                                 // owns a rayon pool: create after the set-up (ADR-0039)
impl Walker {
    pub fn new(opts: WalkOptions) -> Result<Self>;
    pub fn baseline(&mut self, root: &RootPath, ignore: &IgnoreList) -> Result<WalkStats>; // walk, keep, no diff
    pub fn diff(&mut self, root: &RootPath, ignore: &IgnoreList) -> Result<WalkDiff>;      // walk, diff against the last walk, keep
    pub fn forget(&mut self);                      // after emptying the root: next diff adds everything
}
pub struct WalkOptions { pub workers: NonZeroUsize, pub mode: SnapshotMode } // Default: DEFAULT_WALK_WORKERS = 8, Full
pub enum SnapshotMode { Full, Redo, Time }         // kaniko's --snapshot-mode
pub struct WalkDiff { pub root: Option<EntryMeta>, pub changed: Vec<Changed>, pub deleted: Vec<PathBuf>, pub stats: WalkStats }
impl WalkDiff { pub fn is_empty(&self) -> bool; pub fn entries(&self, root: &RootPath) -> Result<Vec<Entry>>;
    pub fn export<W: Write>(&self, root: &RootPath, o: EncodeOptions, out: W) -> Result<(LayerDigests, DiffCounts)>; }
pub struct Changed { pub path: PathBuf, pub change: Change, pub meta: EntryMeta } // Change::{Added, Modified}
pub struct EntryMeta { pub kind: FileType, pub mode, uid, gid, size, mtime, rdev, dev, ino, nlink } // #[non_exhaustive]
pub struct WalkStats { pub entries, files_hashed, bytes_hashed, stat_cache_hits, skipped, max_threads }
```

- **`WalkDiff`** (what the exporter consumes): paths relative to the root,
  never empty, sorted component by component (a directory before its
  contents) whatever the worker count. `changed` lists a directory only when
  its own metadata changed (the exporter adds parent headers); an entry of
  another type than before is `Added`. `deleted` holds the topmost deleted
  paths only (one whiteout each). `root` is set when the root's metadata
  changed and is never written. `meta.dev`/`ino`/`nlink` group hard links.
  Content is read again by the exporter through `RootPath`.
- The walk skips an entry, with everything below it, when
  `IgnoreList::covers` matches it (a literal entry, an `--ignore-path` glob,
  or kaniko's below-only entries); `contains` is an exact match only. The
  walk keeps the previous one as a path arena (`kaniko/tree.rs`); a path
  beyond `u32` offsets is `Error::WalkTooLarge`. A vanished or swapped entry
  is `Error::Changed`. Detail:
  [walk, hash and diff](../../design/kaniko-mode.md#walk-hash-and-diff).
- `Base::Image` takes layers in image order (the config's `diff_ids`, bottom
  first); the lower stack is topmost first. `Base::Stage` shares the parent's
  diffs: release the parent only after its children.
- `base_layer` fills `EntryKey::Layer(<chain ID>)` through `Store::fill`
  (per-digest lock), keyed by chain ID as BuildKit, since a hardlink copied
  from a lower makes an extraction depend on its stack (ADR-0032). It checks
  that the tar hashes to the diff ID, and marks a layer whose tar has no `./`
  entry with `user.ranikust.inherit-root`, so the stage root takes its
  metadata from the layers below, as a flattening extractor would.
- A `mount(2)` mount makes `lowers-<m>/` in the stage directory and mounts
  from a scoped thread that does `unshare(CLONE_FS)` and `fchdir` into it
  (containerd's way, ADR-0032); the stage directory is
  `builds/<id>/stages/<n>/` with `rootfs/`, `diff-<k>/`, `work-<k>/` and
  `empty-<i>/` (a lower for `FROM scratch` and the read-only view, which
  needs two).
- Exported layers keep only the `security.capability` xattr, as BuildKit and
  kaniko (ADR-0032); POSIX ACLs (`system.posix_acl_*`), SELinux labels and
  IMA/EVM signatures a `RUN` sets are dropped too.

## Invariants

- Every overlay mount uses exactly the ADR-0016 set at every privilege level
  and lives in the private mount namespace. Each layer gets a fresh `diff-k`
  and `work-k` (a `volatile` work dir is never remounted; old uppers only
  become lowers), and `diff-k`'s root first takes the topmost lower root's
  mode, uid, gid, mtime and xattrs.
- `layers/<id>` is immutable, only ever a lower, and is published through
  `rnk-store`: extracted into its `tmp/` entry, renamed, per-digest lock.
- Exporter: whiteouts are converted before hardlinks are found by
  `(dev, ino)`; `.wh.<name>` only where the lower stack shows the name, with
  time 0; an entry unchanged by BuildKit's `sameDirent` is left out
  (ADR-0034) yet every emitted entry's ancestors are emitted; an opaque
  directory over a lower one becomes whiteouts of what it hides, never
  `.wh..wh..opq`; `*.overlay.*` xattrs are stripped, `security.capability`
  kept; a redirect is an error; modes are raw; the `rnk-layer` writer sorts,
  so the bytes never depend on thread count
  ([rules](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer)).
- Walk: directories open from the parent fd with `RESOLVE_BENEATH`,
  `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`, `RESOLVE_NO_XDEV`; an entry
  with another `mnt_id` is skipped; `/proc`, `/sys`, `/dev`, the ignore list
  and the state directory (ADR-0022) are never entered. A regular file is
  pinned with `O_PATH`, checked against its `statx`, then reopened through
  `/proc/self/fd`: a FIFO or device node swapped in is never opened. Stat
  cache key `{dev, ino, size, mtime, ctime, mode, uid, gid}`, reused only if
  `max(mtime, ctime)` is over 2 s before the scan started. Known limit:
  xattrs are hashed on regular files and directories only.
- Roots are emptied only through `rnk-rootfs` (`mnt_id`, never `st_dev`);
  per-stage roots are extracted or copied, never hardlinked (ADR-0017).

## Refs

- **Design:** [`design/overlay-snapshot.md#one-layer-per-instruction`](../../design/overlay-snapshot.md#one-layer-per-instruction), [`#mounting`](../../design/overlay-snapshot.md#mounting), [`#the-volatile-lifecycle`](../../design/overlay-snapshot.md#the-volatile-lifecycle), [`#exporting-a-diff-as-an-oci-layer`](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer); [`design/kaniko-mode.md#walk-hash-and-diff`](../../design/kaniko-mode.md#walk-hash-and-diff), [`#stage-roots-on-the-shared-root`](../../design/kaniko-mode.md#stage-roots-on-the-shared-root), [`#per-stage-directories`](../../design/kaniko-mode.md#per-stage-directories), [`#lazy-snapshots`](../../design/kaniko-mode.md#lazy-snapshots); [`design/cache-and-store.md#store-layout`](../../design/cache-and-store.md#store-layout)
- **Decisions:** [ADR-0002](../../adr/0002-two-snapshot-modes.md) (two modes, one interface), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (accepted differences), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (sequential on `/`), [ADR-0016](../../adr/0016-overlay-mount-options.md) (options, `EXDEV`), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md) (no hardlink farms), [ADR-0021](../../adr/0021-rnk-store-crate.md) (`layers/` through `rnk-store`), [ADR-0022](../../adr/0022-common-state-dir.md) (state directory ignored), [ADR-0046](../../adr/0046-snapshotter-signature.md) (the traits)
- **Open:** [#5](https://github.com/tran4774/ranikust/issues/5) (publishing read-only layer roots without root), [#18](https://github.com/tran4774/ranikust/issues/18) (one mount across instructions)
- **Evidence:** [exp-01](../../research/experiments/01-overlay-semantics.md) (upper contents, limits), [exp-06](../../research/experiments/06-overlay-vs-walk.md) (`volatile`), [exp-06a](../../research/experiments/06a-root-rerun.md), [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) (exporter), [exp-09](../../research/experiments/09-kaniko-mode-prototype.md) (walk of 269k entries in 0.10-0.15 s), [exp-10](../../research/experiments/10-exdev-package-managers.md), [exp-11](../../research/experiments/11-root-extras.md), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (kernel 5.15)
- **kaniko:** `kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:116` `Init`, `:237` `scanFullFilesystem` (syncfs, full walk), `:432` `addParentDirectories`; `pkg/snapshot/layered_map.go:28` `LayeredMap`, `:156` `CheckFileChange`; `pkg/util/stat_hash_cache.go:41` the 2 s window, `:54` `statKey`; `pkg/filesystem/resolve.go:39` `ResolvePaths` (2.48 s of a W1 build, exp-07); `pkg/executor/build.go:2179` `DeleteFilesystem` after each stage
- **External:** BuildKit `WriteUpperdir` <https://github.com/moby/buildkit/blob/master/util/overlay/overlay_linux.go>, <https://docs.kernel.org/filesystems/overlayfs.html>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers). Many tests
are red-team cases named `red_team_*`; each pins a finding that was fixed.

- **T0, overlay:** exporter rules on a modelled upper and lower stack
  (`src/export/tests.rs`), lookups, extraction, spans (`tests/spans.rs`,
  `tests/layers.rs`).
- **T0, walk and exporter** (`src/kaniko/walk/tests.rs`,
  `src/kaniko/export/tests.rs`): first diff adds all in path order,
  `baseline` and `forget`, what each `--snapshot-mode` compares, a type
  change in every mode, topmost deletions, symlinks hashed by target, hard
  links sharing `(dev, ino)`, ignored paths neither walked nor deleted, 1
  against 8 workers the same digest, the stat cache and its racy window, a
  FIFO listed and never opened, a file swapped in refused on the pinned fd, a
  tree deeper than `PATH_MAX`; export: an unchanged root gives an empty tar
  (ADR-0058), a deleted directory one whiteout, a deep new file its parents,
  links into a lower layer a regular file, a file swapped in since the walk
  refused, and the property `apply(base layer, diff layer) == tree` on random
  trees. `tests/memory.rs`: a deep chain is refused past `PATH_MAX` with
  linear memory (T15).
- **T0, shared root** (`src/kaniko/root/tests.rs`, `tests/keep.rs`): a kept
  stage read after the root is reused, only the paths asked for, a stage
  `FROM` a kept one, `COPY --from` image unpacked beside the root, the `/`
  layout with its keep area under an ignored `state/`; red team of the keep
  area (hostile layers and roots, links out, `..`, FIFOs, a tree beyond
  `PATH_MAX`) and of the `/ranikust` marker (ADR-0052 D1: planted files,
  links and FIFOs refused before `/` changes; the second build empties `/`
  but never `/.dockerenv`, `/run/.containerenv`, a volume, the state
  directory or the marker).
- **T0, per-stage directories** (`src/kaniko/stages/tests.rs`, ADR-0059,
  ADR-0061, ADR-0062): a private directory per stage (`0700`, root `0755`),
  `FROM <stage>` copied with no inode shared, a parent with a layer open
  refused, a failed diff ID check leaves no directory, an unneeded stage
  never walked; the hand-over of the only reader gives what a copy gives and
  the same layer digest, falls back to a copy when it moved nothing or a
  hardlink leaves the root, and keeps no xattr a copy leaves out; hostile
  bases cannot reach outside `rootfs/`.
- **T1** (user namespace): a layer per instruction; `EINVAL` on a reused
  `volatile` work dir; `/` stays 555; 500 lowers; long option strings.
- **T2c** (rootful Docker, `sysadmin`, `just test-container unit-c`;
  `#[ignore = "T2c: ..."]`): `tests/overlay.rs` (overlay mounts: the probe,
  a layer per instruction with `lowerdir+` and with `mount(2)`, many lowers
  from a deep state directory, a scratch stage), a device node swapped in
  during the walk never opened, a hand-over never giving device nodes, a
  mount below a stage root crossed by neither `release` nor a hand-over.
- **T2** (VM; `just test-vm unit`): the reference before a merge touching
  overlay mounts or deletion; kaniko mode on `/`, flattened content against
  kaniko, BuildKit and the other mode.
- **To do:** benchmark `--snapshot-mode=redo|time` against `full` on W1-W3:
  they skip content reads and can miss a change, so they stay only if
  significantly faster (ADR-0023, ADR-0049).

## Metrics

Rows: [design/metrics.md](../../design/metrics.md#rnk-snapshot).

- Spans: `prepare` (`layers`), `mount`, `umount`, `empty`, `keep` (`paths`; 0
  for the whole root), `hand_over` (inside `prepare` with `handed_over =
  true`, `rnk_rootfs.prune` in it), `release`, `probe`; records `diff`
  (`files_added`, `whiteouts`, `bytes`, ...), `walk` (`entries`,
  `files_hashed`, `bytes_hashed`, `stat_cache_hits`, `max_threads`),
  `baseline` (`entries`), `export` (`entries`, `bytes_read`).
- Overlay records `rnk_snapshot.diff` inside `export`, which is the walk that
  classifies the entries, so a diff never exported is not recorded. The kaniko
  exporter (`WalkDiff::export`) records the same pair: `export` around `diff`
  (`files_added`, `whiteouts`, `bytes`; the others 0), which there only turns
  the walk's diff into entries.
- Walk workers count locally; totals are recorded once per walk.
  `blocks_read` is not recorded yet; `rnk_snapshot.extract` is not a span:
  base layers nest `rnk_store.fill`, `rnk_layer.decode` and
  `rnk_rootfs.extract`.

## Pitfalls

- Without `volatile` every copy-up fsyncs: 12.92 s instead of 0.37 s (exp-06).
- Reusing a `volatile` upper/work pair is `EINVAL` (exp-11 finding 6).
- An upper root made by `mkdir` turns `/` from 555 into 755 (exp-09 finding 1).
- `redirect_dir=off` is `EINVAL` in a user namespace and with `userxattr`,
  even as root (exp-01 finding 1, exp-06a finding 2).
- Overlay nests two deep at most; an upper cannot be on overlayfs (exp-11);
  `mount(2)` data over 4,096 bytes gives `ENOENT` (exp-01 finding 6); kernel
  5.15 fails `lowerdir+` only at `FSCONFIG_CMD_CREATE` (exp-13 finding 5).
- Whiteouts share one inode (`nlink=4`) (exp-01 finding 2); missing parent
  headers let extractors recreate `/root` and `/tmp` as 755 (exp-07).
- A state directory inside the walked root leaks into the diff unless
  ignored; 32 walk workers were slower than 8 (exp-09).
- `rnk-sandbox` places stub mount targets in the stage root and removes them
  after the `RUN`, restoring the parent's times; on an overlay root the stub
  copies its parent (`/etc`) up, so the upper keeps a directory whose
  metadata equals the lower's. The exporter must leave such directories out,
  or `/etc` lands in every layer [inferred] (exp-12).
- After a crash the overlay mount is gone, so the sandbox's journal replay
  cannot find stubs left in an upper; discard that upper with its build,
  never reuse it [inferred].
- Adding an entry to a copied-up directory moves its mtime, so it is exported
  as changed; only a copy-up that nothing below changed is skipped. Tests
  that fake a copy-up must set the times last.
- The new upper root must not take the lower root's overlay xattrs (a copied
  `user.overlay.opaque=y` would hide every lower layer), and its own stay in
  the overlay form: escaped `user.overlay.overlay.*` stays escaped, or an
  image's xattr would become overlay's own marker.
- A container's own root is overlayfs, which overlay refuses as an upper
  directory: overlay mode in a container needs the state directory on a
  volume (the smoke suites mount one); otherwise the probe refuses and the
  outcome is kaniko mode.
- Publishing renames a layer directory to another parent, which needs write
  permission on it: a `0555` layer root works only as root ([open: #5]).
- Unmounting is `MNT_DETACH`: open descriptors below the mount (a
  `RootPath`) do not make it busy, and the upper is complete once nothing
  writes.
- **Contract for `rnk-builder` and `rnk-sandbox`:** every process of a `RUN`
  is dead (reaped, its PID namespace gone) before `end_layer`: `MNT_DETACH`
  leaves a surviving writer able to keep writing into the upper while it is
  exported or used as a lower.
- **Contract for `rnk-sandbox`:** its stubs (`/etc/resolv.conf`, `/etc/hosts`,
  `/etc/hostname`, mount targets) are journalled and removed from the upper,
  with the parent's times restored, before the diff is exported; the exporter
  has no ignore list for them.
- The state directory's path must not hold `,` `:` `\` `=` or control
  characters: `mount(2)` would read them as option syntax (a state directory
  `/x,lowerdir=/` would add the host's `/` as a lower). `Overlay::new` and
  `probe` refuse such a path.
- A stage built on another (`Base::Stage`) holds its parent's diffs as
  lowers: the parent's `release` refuses while the child lives.
