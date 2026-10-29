# rnk-snapshot

> **Status:** skeleton · **Phase:** 1 ·
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

## API (planned)

```rust
pub trait StageRootfs {                            // [design] survey 06 §5.6
    fn path(&self) -> &RootPath;
    fn begin_layer(&mut self) -> Result<()>;       // overlay: diff-k, work-k, root metadata, mount
    fn end_layer(&mut self) -> Result<LayerDiff>;  // overlay: unmount, diff-k; kaniko: walk + hash
    fn release(self) -> Result<()>;                // overlay: delete diff-*; kaniko: safe delete
    fn sharing(&self) -> RootSharing;              // [design] Shared (`/`, runs alone) | Private
}
pub trait Snapshotter {                            // [open: Q-024] named in ADR-0002; signature designed at Phase 1 task 9
    type Root: StageRootfs;
    fn prepare(&self, base: &[LayerRef], parent: Option<&Self::Root>) -> Result<Self::Root>;
    fn final_root(&self, stage: &Self::Root) -> Result<RootPath>;  // [design] read-only, for COPY --from
}
pub struct LayerDiff;  // [design] entries with OCI whiteouts, read lazily; rnk-layer sorts and encodes them
pub fn probe_overlay(dir: &Path) -> OverlayProbe;  // [design] ADR-0016 mount; lowerdir+ by a real create
pub const OVERLAY_OPTIONS: &str =                  // [decided] ADR-0016
    "userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile";
```

## Invariants

- Every overlay mount uses exactly the ADR-0016 set at every privilege level
  and lives in the private mount namespace. Each layer gets a fresh `diff-k`
  and `work-k` (a `volatile` work dir is never remounted; old uppers only
  become lowers), and `diff-k`'s root first takes the topmost lower root's
  mode, uid, gid, mtime and xattrs.
- `layers/<id>` is immutable, only ever a lower, and is published through
  `rnk-store`: extracted into its `tmp/` entry, renamed, per-digest lock.
- Exporter: whiteouts are converted before hardlinks are found by
  `(dev, ino)`; `.wh.<name>` and `.wh..wh..opq` only where a lower has the
  name; an unchanged copied-up directory is no change by itself, yet every
  emitted entry's ancestors are emitted; `*.overlay.*` xattrs are stripped,
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

## Phase 1

- Build, in order: traits → `probe_overlay` → extraction into `layers/` via
  `rnk-store` → lower stack, `begin_layer`/`end_layer` as root → `lowerdir+`,
  then `mount(2)` with `l/<short>` → exporter → `release` → `final_root`.
- Deferred: the kaniko implementation, per-stage directories and lazy
  snapshots (Phase 2); mounting in a user namespace (Phase 3).
- [open: Q-024] The `Snapshotter` signature (task 9). [open: Q-026]
  Merging layers near 500 lowers. [open: Q-049] One mount across
  instructions; the export-time hash filter.
