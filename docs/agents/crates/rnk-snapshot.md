# rnk-snapshot

> **Status:** overlay mode in progress (Phase 1 task 9) · **Phase:** 1 ·
> **Depends on:** `rnk-rootfs`, `rnk-layer`, `rnk-store` · **Used by:** `rnk-builder`, `rnk-sched` (root sharing only), `ranikust` (probes) · **Path:** `crates/rnk-snapshot/`

**TL;DR:** Gives each stage a root (`StageRootfs`) and turns each change into
a `LayerDiff`: overlay mode mounts a fresh upper per instruction, kaniko mode
walks and hashes. Keep `volatile` with fresh work dirs, the lower root's
metadata on the upper root, whiteouts before hardlinks; delete via `rnk-rootfs`.

## Owns

- The `StageRootfs` and `Snapshotter` traits and their two implementations.
- Overlay: mounts with the ADR-0016 options, one upper per instruction,
  overlay → OCI whiteout conversion, base layers extracted into the store's
  `layers/` (`rnk-layer` decoder → `rnk-rootfs::extract`), the exporter that
  feeds `rnk-layer`'s writer, and the overlay and `lowerdir+` probes.
- kaniko: parallel walk and hash with a stat cache, `LayeredMap`, path
  resolution, filesystem deletion (what and when), per-stage directories.

## Must not

- Run instructions (`rnk-sandbox`, `rnk-builder`), encode layers (`rnk-layer`),
  expose a stage root as anything but a `RootPath`, or delete by path.
- Own the store (`rnk-store`), transfer blobs (`rnk-registry`), compute
  cache keys (`rnk-cache`), or switch modes mid-build.

## API

```rust
pub trait StageRootfs {                            // [open: Q-024] interim, task 9
    fn root(&self) -> Result<&RootPath>;           // writable root of the open layer (COPY, ADD, WORKDIR)
    fn mount_point(&self) -> &Path;                // host path, only for rnk-sandbox to enter for RUN
    fn begin_layer(&mut self) -> Result<()>;       // overlay: diff-k, work-k, root metadata, mount
    fn end_layer(&mut self) -> Result<LayerDiff>;  // overlay: unmount; diff-k becomes the top lower
    fn view(&mut self) -> Result<&RootPath>;       // read-only between layers (USER lookup, COPY --from)
    fn release(self) -> Result<()>;                // unmount, delete the stage directory, diffs included
}
pub trait Snapshotter {                            // [open: Q-024] interim, task 9
    type Root: StageRootfs;
    fn base_layer<R: Read>(&self, diff_id: &Digest, blob: R, below: &[LayerDir]) -> Result<LayerDir>;
    fn prepare(&self, base: Base<'_, Self::Root>) -> Result<Self::Root>;  // Scratch | Image(&[LayerDir]) | Stage(&Root)
}
impl Overlay { pub fn new(store: Arc<Store>, caps: OverlayCaps) -> Result<Self>; pub fn store(&self) -> &Arc<Store>; }
// Overlay: Snapshotter<Root = OverlayStage>
pub struct LayerDir;                               // layers/<algo>/<hex>/: diff_id(), path()
pub struct LayerDiff;                              // upper + lowers; lazily exported
impl LayerDiff {
    pub fn new(upper: PathBuf, lowers: Vec<PathBuf>) -> Self;
    pub fn is_empty(&self) -> Result<bool>;
    pub fn entries(&self) -> Result<Vec<rnk_layer::Entry>>;
    pub fn export<W: Write>(&self, o: EncodeOptions, out: W) -> Result<(LayerDigests, DiffCounts)>;
}
pub fn probe(dir: &Path) -> Result<OverlayCaps>;   // ADR-0016 mount; lowerdir+ else mount(2); write-through check
pub struct OverlayCaps { pub api: MountApi }       // MountApi::{LowerdirPlus, Mount2}
pub const OVERLAY_OPTIONS: &str =                  // [decided] ADR-0016
    "userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile";
pub const MAX_LOWERS: usize = 500;                 // (Q-026, ADR-0032) refused beyond, no merging yet
```

- `Base::Image` takes layers in image order (the config's `diff_ids`,
  bottom first); the lower stack is topmost first. `Base::Stage` shares
  the parent's diffs: release the parent only after its children.
- `base_layer` fills `EntryKey::Layer(<chain ID>)` through `Store::fill`
  (per-digest lock): keyed by chain ID, as BuildKit, since a hardlink
  copied from a lower makes an extraction depend on its stack (ADR-0032,
  Q-150). It checks that the tar hashes to the diff ID, and marks a layer
  whose tar has no `./` entry with `user.ranikust.inherit-root`, so the
  stage root takes its metadata from the layers below, as a flattening
  extractor would.
- The stage directory is `builds/<id>/stages/<n>/` with `rootfs/`,
  `diff-<k>/`, `work-<k>/` and `empty-<i>/` (a lower for `FROM scratch`
  and the read-only view, which needs two). A `mount(2)` mount makes
  `lowers-<m>/` there with links `0`, `1`, ... to its lowers, mounts from
  a scoped thread that does `unshare(CLONE_FS)` and `fchdir` into it
  (containerd's way, ADR-0032, Q-151), and removes it again.
- Exported layers keep only the `security.capability` xattr, as BuildKit
  and kaniko (ADR-0032, Q-025). POSIX ACLs (`system.posix_acl_*`), SELinux
  labels and IMA/EVM signatures (`security.ima`, `security.evm`) a `RUN`
  sets are dropped too, as there.

## Invariants

- Every overlay mount uses exactly the ADR-0016 set at every privilege level
  and lives in the private mount namespace. Each layer gets a fresh `diff-k`
  and `work-k` (a `volatile` work dir is never remounted; old uppers only
  become lowers), and `diff-k`'s root first takes the topmost lower root's
  mode, uid, gid, mtime and xattrs.
- `layers/<id>` is immutable, only ever a lower, and is published through
  `rnk-store`: extracted into its `tmp/` entry, renamed, per-digest lock.
- Exporter: whiteouts are converted before hardlinks are found by
  `(dev, ino)`; `.wh.<name>` only where the lower stack shows the name,
  with time 0; an entry unchanged by BuildKit's `sameDirent` is left out
  (ADR-0034; contents compared when both times are whole seconds, and
  over this build's own diffs whenever the stat matches), yet
  every emitted entry's ancestors are emitted; an opaque directory over a
  lower one becomes whiteouts of what it hides, never `.wh..wh..opq`; `*.overlay.*` xattrs are stripped,
  `security.capability` kept; a redirect is an error; modes are raw; the
  `rnk-layer` writer sorts, so the bytes never depend on thread count.
- Walk: directories open from the parent fd with `RESOLVE_BENEATH`,
  `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`, `RESOLVE_NO_XDEV`; an entry
  with another `mnt_id` is skipped; `/proc`, `/sys`, `/dev` and the ignore
  list are never entered; nor is the state directory (ignored, ADR-0022).
- Stat cache key `{dev, ino, size, mtime, ctime, mode, uid, gid}`, reused only
  if `max(mtime, ctime)` is over 2 s before the scan started. A deletion is
  one whiteout at the topmost deleted path.
- Roots are emptied only through `rnk-rootfs` (`mnt_id`, never `st_dev`);
  per-stage roots are extracted or copied, never hardlinked (ADR-0017).

## Refs

- **Design:** [`design/overlay-snapshot.md#one-layer-per-instruction`](../../design/overlay-snapshot.md#one-layer-per-instruction), [`#mounting`](../../design/overlay-snapshot.md#mounting), [`#the-volatile-lifecycle`](../../design/overlay-snapshot.md#the-volatile-lifecycle), [`#exporting-a-diff-as-an-oci-layer`](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer); [`design/kaniko-mode.md#walk-hash-and-diff`](../../design/kaniko-mode.md#walk-hash-and-diff), [`#per-stage-directories`](../../design/kaniko-mode.md#per-stage-directories), [`#lazy-snapshots`](../../design/kaniko-mode.md#lazy-snapshots); [`design/cache-and-store.md#store-layout`](../../design/cache-and-store.md#store-layout)
- **Decisions:** [ADR-0002](../../adr/0002-two-snapshot-modes.md) (two modes, one interface), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (accepted differences), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (sequential on `/`), [ADR-0016](../../adr/0016-overlay-mount-options.md) (options, `EXDEV`), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md) (no hardlink farms), [ADR-0021](../../adr/0021-rnk-store-crate.md) (`layers/` through `rnk-store`), [ADR-0022](../../adr/0022-common-state-dir.md) (state directory ignored)
- **Evidence:** [exp-01](../../research/experiments/01-overlay-semantics.md) (upper contents, limits), [exp-06](../../research/experiments/06-overlay-vs-walk.md) (`volatile`), [exp-06a](../../research/experiments/06a-root-rerun.md), [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) (exporter), [exp-09](../../research/experiments/09-kaniko-mode-prototype.md) (walk of 269k entries in 0.10–0.15 s), [exp-10](../../research/experiments/10-exdev-package-managers.md), [exp-11](../../research/experiments/11-root-extras.md), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md), [exp-13](../../research/experiments/13-docker-rootful-vm.md) (kernel 5.15)
- **kaniko:** `kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:116` `Init`, `:237`
  `scanFullFilesystem` (syncfs, full walk), `:432` `addParentDirectories`;
  `pkg/snapshot/layered_map.go:28` `LayeredMap`, `:156` `CheckFileChange`;
  `pkg/util/stat_hash_cache.go:41` the 2 s window, `:54` `statKey`;
  `pkg/filesystem/resolve.go:39` `ResolvePaths` (2.48 s of a W1 build,
  exp-07); `pkg/executor/build.go:2179` `DeleteFilesystem` after each stage.
- **External:** BuildKit `WriteUpperdir` <https://github.com/moby/buildkit/blob/master/util/overlay/overlay_linux.go>, <https://docs.kernel.org/filesystems/overlayfs.html>

## Tests

- **T0:** exporter rules on a modelled upper and lower stack; kaniko walk on
  temp dirs: property `apply(base, diff) == tree`, the racy window, an
  unchanged `RUN` giving an empty layer.
- **T1** (user namespace): a layer per instruction; `EINVAL` on a reused
  `volatile` work dir; `/` stays 555; 500 lowers; long option strings.
- **T2** (VM only): overlay as real root; kernel 5.15 fallback; kaniko mode
  on `/`; flattened content against kaniko, BuildKit and the other mode.

## Metrics

- Record `rnk_snapshot.diff`; `mount`, `export` (`blocks_read`) and more;
  Phase 2 walk spans ([rows](../../design/metrics.md#rnk-snapshot)).
- Overlay records `rnk_snapshot.diff` inside `export`, which is the walk
  that classifies the entries, so a diff never exported is not recorded.
  `blocks_read` is not recorded yet; `rnk_snapshot.extract` is not a span:
  base layers nest `rnk_store.fill`, `rnk_layer.decode` and
  `rnk_rootfs.extract`.
- Walk workers count locally; totals are recorded once per walk.

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
- `rnk-sandbox` places stub mount targets (`/etc/resolv.conf`, `/proc`) in
  the stage root and removes them after the `RUN`, restoring the parent's
  times; on an overlay root the stub copies its parent (`/etc`) up, so the
  upper keeps a directory whose metadata equals the lower's. The exporter
  must leave such directories out, or `/etc` lands in every layer [inferred]
  (host-safety review of `rnk-sandbox`; exp-12).
- After a crash the overlay mount is gone, so the sandbox's journal replay
  cannot find stubs left in an upper; that upper must be discarded with its
  build, never reused [inferred].
- Adding an entry to a copied-up directory moves its mtime, so it is
  exported as changed; only a copy-up that nothing below changed is
  skipped. Tests that fake a copy-up must set the times last.
- The new upper root must not take the lower root's overlay xattrs: a
  copied `user.overlay.opaque=y` would hide every lower layer.
- A container's own root is overlayfs, which overlay refuses as an upper
  directory: overlay mode in a container needs the state directory on a
  volume (the smoke suites mount one); otherwise the probe refuses and the
  outcome is kaniko mode.
- Publishing renames a layer directory to another parent, which needs
  write permission on it: a `0555` layer root works only as root
  ([open: Q-152]).
- Unmounting is `MNT_DETACH`: open descriptors below the mount (a
  `RootPath`) do not make it busy, and the upper is complete once
  nothing writes.
- **Contract for `rnk-builder` and `rnk-sandbox`:** every process of a
  `RUN` is dead (reaped, its PID namespace gone) before `end_layer`.
  `MNT_DETACH` leaves a surviving writer able to keep writing into the
  upper while it is exported or used as a lower.
- **Contract for `rnk-sandbox`:** its stubs (`/etc/resolv.conf`,
  `/etc/hosts`, `/etc/hostname`, mount targets) are journalled and removed
  from the upper, with the parent's times restored, before the diff is
  exported; the exporter has no ignore list for them.
- Root xattrs of the new upper stay in the overlay form: escaped
  `user.overlay.overlay.*` stays escaped. Un-escaping it there would hand
  an image's xattr to overlay as its own marker (host-safety review).
- The state directory's path must not hold `,` `:` `\` `=` or control
  characters: `mount(2)` would read them as option syntax (a state
  directory `/x,lowerdir=/` would add the host's `/` as a lower).
  `Overlay::new` and `probe` refuse such a path.
- A stage built on another (`Base::Stage`) holds its parent's diffs as
  lowers: the parent's `release` refuses while the child lives.

## Phase 1

- Done (`feat/rnk-snapshot`): traits, `probe`, extraction into `layers/`
  via `rnk-store`, lower stack and `begin_layer`/`end_layer` as root with
  `lowerdir+` and the `mount(2)` fallback, the exporter, `view`,
  `release`; `ranikust` runs `probe` at set-up. T0 for the exporter,
  lookups, extraction and spans; T2 (`tests/overlay.rs`) for real mounts.
- Left: the builder's use (task 11): lazy export, `RUN` in
  `mount_point`, `COPY` through `root`; `blocks_read` on
  `rnk_snapshot.export`; the "unchanged `RUN` gives an empty layer" rule
  against kaniko; T1 mounting in a user namespace (Phase 3).
- Deferred: the kaniko implementation, per-stage directories and lazy
  snapshots (Phase 2); mounting in a user namespace (Phase 3).
- [open: Q-024] The signature (interim above). (Q-026, ADR-0032) Merging
  layers near 500 lowers. [open: Q-245] One mount across instructions. [open: Q-152].
