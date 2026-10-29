# rnk-store

> **Status:** in progress (Phase 1 part done) · **Phase:** 1 ·
> **Depends on:** `rnk-rootfs` (deletion, every path), `rnk-oci` (`Digest`); `sha2` (cache ids) · **Used by:** `rnk-registry`, `rnk-snapshot`, `rnk-cache`, `rnk-builder`, `ranikust` (`prune`) · **Path:** `crates/rnk-store/`

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

## API

Built (`feat/rnk-store`) unless marked:

```rust
pub struct BuildPaths { pub root: PathBuf, pub state: PathBuf }  // root = workdir; state = --state-dir
impl BuildPaths { pub fn new(root: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self; }
pub struct Store { /* state dir RootPath, build id, build lock fd */ }  // Send + Sync
impl Store {
    pub fn open(paths: &BuildPaths) -> Result<Store>;   // mkdir -p, canonicalize, marker check, build lock, clean-up
    pub fn state_dir(&self) -> &Path;                   // canonical
    pub fn build_id(&self) -> &str;
    pub fn build_dir(&self) -> &Path;                   // builds/<id>, created by open
    pub fn get(&self, key: &EntryKey) -> Result<Option<PathBuf>>;  // a published entry; no lock
    pub fn fill<F, E>(&self, key: &EntryKey, f: F) -> Result<PathBuf>  // lock → published? return it →
        where F: FnOnce(&TmpEntry) -> std::result::Result<u64, E>,    //   tmp/<id>/<n> → f (bytes written)
              E: Into<Box<dyn std::error::Error + Send + Sync>>;      //   → fsync a blob → rename
    pub fn cache_dir(&self, id: &str) -> Result<PathBuf>;  // caches/<sha256(id)>, created if missing
    pub fn close(self) -> Result<()>;                   // removes builds/<id> and tmp/<id>
    pub fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport>; // [design] Phase 5, [open: Q-043]
}
pub struct TmpEntry;  // path() -> &Path; file() -> Option<&File> (blob); root() -> Option<&RootPath> (layer)
pub enum EntryKey { Blob(Digest), Layer(Digest) }  // Layer by diff ID [open: Q-015]; digest() -> &Digest
pub enum Error { NotAStateDir, NoFlock, Io, Rootfs, Fill }
```

`f` must return `Ok` only for a verified entry: the store does not hash a
blob again. It is synchronous; an async caller (`rnk-registry`) runs `fill`
on a blocking thread, because the lock may wait.

## Invariants

- An entry appears only by `rename` from `tmp/`, on the same filesystem,
  after it was verified (blob digest; layer extracted completely). Published
  entries are immutable and never rewritten (ADR-0006, threat T13).
- The per-digest lock is taken before looking for the entry, so two builds
  never fetch or extract one digest at once; a waiter finds it published.
- Locks are `flock` on lock files in the state directory, released by the
  kernel when the process dies; no `flock` support is a start-up error
  (`Error::NoFlock`). Per-digest lock files are
  `locks/{blobs,layers}/<algorithm>/<hex>` and are never removed (until
  `prune`), so no lock is ever taken on an unlinked file.
- A build is alive while it holds the lock on `builds/<id>.lock`; it
  takes it before creating `builds/<id>` and `tmp/<id>`, so a clean-up never
  sees the directories of a build that is still registering.
- A crash leaves only `tmp/<id>` entries and `builds/<id>`; both go at the
  next start (T0 test with `SIGKILL`). An upper written under `volatile` is
  never reused.
- A non-empty state directory without a regular `.ranikust-root` file is
  refused, so clean-up never runs in a directory the user pointed at by
  mistake.
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

- **T0** (temp dirs, built): `tests/store.rs`: layout and marker, a
  foreign directory refused, `fill` publishes once, a failing `f`
  publishes nothing, concurrent `fill` from threads runs `f` once, leftovers
  of a dead build removed and a running build's kept, `close`, `cache_dir`;
  `tests/processes.rs`: four processes filling one digest run `f` once, a
  build killed by `SIGKILL` inside `f` leaves nothing and frees the lock;
  `tests/spans.rs`. To come with Phase 5: `prune` by size and age.
- **T2** (VM only): two builds as real root sharing one store, with a
  layer extracted through `rnk-rootfs::extract` (device nodes, foreign
  owners); comes with `rnk-snapshot` (task 9), which fills layers.

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
  shares until `exec` closes it (`O_CLOEXEC`): while another thread forks
  (the `RUN` spawner, a test spawning processes), a dead build's lock can
  look held, and clean-up skips that build until the next start. Harmless
  (it only delays clean-up), but it made a clean-up test flaky beside a
  test that spawns processes; those live in their own test binary.
- Each lock gets its own descriptor: two threads of one process exclude
  each other only because their `flock`s are on different open file
  descriptions.
- Layer entries hold image content that may be unwritable to an
  unprivileged user (a `0555` directory); deletion then fails with
  `EACCES`, and clean-up warns and leaves the build for the next start.
- Deleting `builds/<id>` meets image content and may meet mounts: fd-based,
  by `mnt_id`, through `rnk-rootfs` (exp-09 D2b).

## Phase 1

- Task 7 of [phase-1.md](../phase-1.md#task-order), after `rnk-rootfs`,
  `rnk-layer` and `rnk-registry` (tasks 4, 5, 6).
- Order: `BuildPaths` and the layout → `fill` with per-digest locks and
  `rename` → start-up clean-up → `cache_dir` and `build_dir`. Done; the
  caller (`ranikust`, task 8) still has to open the store at set-up and
  `close` it at the end.
- Deferred: `prune` (Phase 5).
- [open: Q-015] Interim: layers named by diff ID, lock files under
  `locks/`. [open: Q-100] Durability: a blob is `fsync`ed before its
  rename, a layer is not. [open: Q-011] The owner of the workdir checks
  and the kaniko-mode workdir lock (`ranikust` or `rnk-rootfs`); not here.
