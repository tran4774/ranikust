# rnk-store

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** `rnk-rootfs` (deletion), `rnk-oci` (`Digest`) [design] · **Used by:** `rnk-registry`, `rnk-snapshot`, `rnk-cache`, `rnk-builder`, `ranikust` (`prune`) · **Path:** `crates/rnk-store/`

**TL;DR:** The persistent, content-addressed store in the state directory:
`blobs/`, `layers/`, `caches/`, `tmp/`, with per-digest locks, atomic
rename, start-up clean-up and `prune`, plus `BuildPaths`. Callers fill an
entry; this crate publishes it whole or not at all, and never lets two
builds fetch or extract the same digest at once.

## Owns

- The store layout in the state directory (ADR-0022): `blobs/sha256/`,
  `layers/<id>/`, `caches/<sha256(id)>/`, `tmp/`, lock files, the
  per-build `builds/<id>/` areas and the `.ranikust-root` marker.
- Per-digest `flock` locks; publication of a filled temporary entry by
  atomic `rename`.
- Start-up clean-up of `tmp/` leftovers and of `builds/<id>` whose build is
  gone; `prune` by total size and age. `ranikust prune` only calls it.
- `BuildPaths`, the paths handed down to every crate.

## Must not

- Decide what an entry contains: `rnk-registry` fills a blob, `rnk-snapshot`
  an extracted layer (`rnk-layer` decodes, `rnk-rootfs::extract` writes).
- Transfer blobs (`rnk-registry`), decode or encode layers (`rnk-layer`),
  mount or diff (`rnk-snapshot`), or compute cache keys (`rnk-cache`).
- Write outside the state directory, or delete except through `rnk-rootfs`:
  entries hold untrusted image content.

## API (planned)

```rust
pub struct BuildPaths { pub root: PathBuf, pub state: PathBuf }  // [design] root = workdir; state = --state-dir
pub struct Store { /* state dir fd, build id, lock files */ }      // [design]
impl Store {
    pub fn open(paths: &BuildPaths) -> Result<Store>;            // [design] marker check, then start-up clean-up
    pub fn get(&self, key: &EntryKey) -> Result<Option<PathBuf>>; // [design] a published entry only
    pub fn fill<F>(&self, key: &EntryKey, f: F) -> Result<PathBuf> // [design] lock → published? return it →
        where F: FnOnce(&TmpEntry) -> Result<()>;                 //   tmp/<random> → f → verify → rename
    pub fn cache_dir(&self, id: &str) -> Result<PathBuf>;         // [design] caches/<sha256(id)>, RUN --mount=type=cache
    pub fn build_dir(&self) -> &Path;                              // [design] builds/<id>, removed at the end
    pub fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport>; // [open: Q-043] defaults, age measure, in-use tracking
}
pub enum EntryKey { Blob(Digest), Layer(Digest) }  // [open: Q-015] which digest names layers/<id>
```

## Invariants

- An entry appears only by `rename` from `tmp/`, on the same filesystem,
  after it was verified (blob digest; layer extracted completely). Published
  entries are immutable and never rewritten (ADR-0006, threat T13).
- The per-digest lock is taken before looking for the entry, so two builds
  never fetch or extract one digest at once; a waiter finds it published.
- Locks are `flock` on lock files in the state directory, released by the
  kernel when the process dies [inferred]; no `flock` support is a start-up
  error [inferred].
- A crash leaves only `tmp/` entries and `builds/<id>`; both go at the next
  start. An upper written under `volatile` is never reused.
- `prune` takes the same locks as a build and never removes an entry a
  running build uses [inferred: in-use tracking is not designed].
- In kaniko mode the state directory (`/ranikust/state` in a container) is
  on the ignore list: nothing walks, snapshots or deletes it as part of a
  root.
- Every descriptor is `O_CLOEXEC`; none reaches `RUN` (threat T7).

## Refs

- **Design:** [`design/cache-and-store.md#store-layout`](../../design/cache-and-store.md#store-layout), [`#locks`](../../design/cache-and-store.md#locks), [`#atomic-publication`](../../design/cache-and-store.md#atomic-publication), [`#prune`](../../design/cache-and-store.md#prune), [`#ownership`](../../design/cache-and-store.md#ownership), [`#failure-handling`](../../design/cache-and-store.md#failure-handling); [`design/overlay-snapshot.md#state-directory-layout`](../../design/overlay-snapshot.md#state-directory-layout); [threat model T7, T13, T14](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0021](../../adr/0021-rnk-store-crate.md) (this crate), [ADR-0022](../../adr/0022-common-state-dir.md) (one state directory, defaults, workdir lock in kaniko mode only), [ADR-0006](../../adr/0006-persistent-store.md) (persistent, locks, rename, prune), [ADR-0007](../../adr/0007-run-anywhere-shared-store.md) (shared by concurrent builds), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md) (never hardlink out of the store)
- **Evidence:** [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) §5 (parallel extraction 0.37 s against 0.88 s), [exp-09](../../research/experiments/09-kaniko-mode-prototype.md) finding 4 (a state directory in the walked root leaks), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) finding 9 (a hardlink farm corrupts the store)
- **kaniko:** `kaniko@44aa7ddc0 pkg/config/init.go:41` (`KanikoDir`),
  `:49-81` (its per-purpose directories); `pkg/util/fs_util.go:72-89` (the
  ignore list starts with `KanikoDir`). kaniko keeps no store between builds
  and takes no locks.
- **External:** <https://man7.org/linux/man-pages/man2/flock.2.html>, <https://man7.org/linux/man-pages/man2/rename.2.html>

## Tests

- **T0** (temp dirs): `fill` publishes once; concurrent `fill` of one digest
  from threads and from forked processes runs `f` once and the waiter gets
  the entry; a failing or mismatching `f` publishes nothing; leftover
  `tmp/` and `builds/<dead>` are removed by `open`; `prune` by size and age.
- **T2** (VM only): two builds sharing one store; `SIGKILL` during an
  extraction, then a new build.

## Metrics

- `rnk_store.lock` (`contended`; time = lock wait), `rnk_store.fill`
  (`reused`, `bytes_written`, `bytes_reused`), `rnk_store.cleanup`, later
  `rnk_store.prune` ([rows](../../design/metrics.md#rnk-store)).
- Lock waits and reuse show what sharing the store costs and saves.

## Pitfalls

- `rename` across filesystems fails with `EXDEV`: `tmp/` lives in the state
  directory itself (cache-and-store design).
- A state directory inside the walked root leaks into the diff unless it is
  on the ignore list (exp-09 finding 4).
- Hardlinking store files into a stage root lets one `echo >>` change the
  store and every sibling (exp-12 finding 9, ADR-0017).
- An `flock` belongs to the open file description, which a forked child
  shares: open lock files `O_CLOEXEC` and never hold one across a fork
  [inferred].
- Deleting `builds/<id>` meets image content and may meet mounts: fd-based,
  by `mnt_id`, through `rnk-rootfs` (exp-09 D2b).

## Phase 1

- Task 7 of [phase-1.md](../phase-1.md#task-order), after `rnk-rootfs`,
  `rnk-layer` and `rnk-registry` (tasks 4, 5, 6).
- Order: `BuildPaths` and the layout → `fill` with per-digest locks and
  `rename` → start-up clean-up → `cache_dir` and `build_dir`.
- Deferred: `prune` (Phase 5).
- [open: Q-015] The digest that names `layers/<id>`; where lock files
  live. [open: Q-011] The owner of the workdir checks and the kaniko-mode
  workdir lock (`ranikust` or `rnk-rootfs`).
