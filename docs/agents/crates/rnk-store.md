# rnk-store

> **Status:** in progress · **Phase:** 1-3 (`prune_all`: 3; `prune` by size and age: 5) ·
> **Depends on:** `rnk-rootfs` (deletion, every path), `rnk-oci` (`Digest`);
> `sha2` (cache ids) ·
> **Used by:** `rnk-snapshot`, `rnk-builder`, `ranikust` (`prune`),
> `rnk-cache` (planned) · **Path:** `crates/rnk-store/`

**TL;DR:** The persistent, content-addressed store in the state directory:
`blobs/`, `layers/`, `caches/`, `tmp/`, with per-digest locks, atomic rename,
start-up clean-up and `prune`, plus `BuildPaths`. Callers fill an entry; this
crate publishes it whole or not at all, and never lets two builds fetch or
extract the same digest at once.

## Owns

- The store layout in the state directory (ADR-0022): `blobs/sha256/`,
  `layers/<id>/`, `caches/<sha256(id)>/`, `tmp/`, lock files, the per-build
  `builds/<id>/` areas, the `.ranikust-state` marker (`STATE_MARKER`;
  ADR-0050 renamed it from `.ranikust-root`), which `prune_all` renames
  `.ranikust-pruning` (`PRUNING_MARKER`) while it empties the directory;
  the name `sandbox/` (`SANDBOX_DIR`), which `ranikust` makes for the
  `RUN` sandbox and `prune_all` empties with the rest.
- The anonymous store of kaniko mode on `/` (ADR-0051, ADR-0056, ADR-0066,
  ADR-0077): see Invariants.
- Per-digest `flock` locks; publication of a filled temporary entry by atomic
  `rename`.
- Start-up clean-up of `tmp/` leftovers and of `builds/<id>` whose build is
  gone; `prune_all`, the emptying of `ranikust prune --all` (ADR-0105);
  `prune` by total size and age is Phase 5, not built. `ranikust prune`
  only calls it. A user other than root opens the store, and prunes,
  inside the build's user namespace (`ranikust`'s `setup::rootless`, #79,
  on the steps of `setup::enter_user_namespace`, #75, and `prune`, #76),
  where the clean-up and the prune remove files of the user's subordinate
  ids as root of the namespace, and where extracted layers are filled and
  published (#77).
- Telling whether the process runs in a user namespace other than the
  initial one (`user_namespace_proc`), for its own directories' group and
  `rnk-snapshot`'s layer keys and device nodes.
- `BuildPaths`, the paths handed down to every crate.

## Must not

Boundaries: `crates/rnk-store/src/lib.rs`. In short:

- Decide what an entry contains: `rnk-registry` fills a blob, `rnk-snapshot`
  an extracted layer (`rnk-layer` decodes, `rnk-rootfs::extract` writes).
- Transfer blobs (`rnk-registry`), decode or encode layers (`rnk-layer`),
  mount or diff (`rnk-snapshot`), or compute cache keys (`rnk-cache`).
- Write outside the state directory, or delete except through `rnk-rootfs`:
  entries hold untrusted image content.

## API

```rust
pub struct BuildPaths { pub root: PathBuf, pub state: PathBuf }  // root = `/` (kaniko mode in a container) or the state dir; state = --state-dir (ADR-0050)
pub fn user_namespace_proc() -> Result<Option<OwnedFd>>; // /proc/self (procfs checked) in a user namespace other than the initial one, told by the inode of ns/user (nsfs checked), not by the map; None in the initial one; rnk-snapshot reads the maps there
impl BuildPaths { pub fn new(root: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self; }
pub struct Store { /* state dir RootPath, build id, build lock fd */ }  // Send + Sync
impl Store {
    pub fn open(paths: &BuildPaths) -> Result<Store>;   // mkdir -p, canonicalize, marker check, build lock, clean-up
    pub fn check_state_dir(state: &Path) -> Result<StateDirHold>; // open's lock, marker and BUILDING checks, writing nothing: before rootless overlay mode's probe (Q-111, (a)); the hold keeps the shared lock until the caller's Store::open has its own
    pub fn open_anonymous(state: &Path) -> Result<Store>; // kaniko mode on /: nothing on disk (ADR-0051); layers and cache mounts refused (Error::Anonymous); a persistent store refused (StateNotEmpty, ADR-0077) unless it holds no data, then emptied first under the lock as abandon does (ADR-0136)
    pub fn is_anonymous(&self) -> bool;
    pub fn files_in_memory(&self) -> bool;              // the anonymous files made now are memfds (can turn true, never back); kaniko mode on / then keeps no spool for later stages (ADR-0081)
    pub fn anonymous_file(&self) -> Result<(File, PathBuf)>; // a nameless file kept as long as the store
    pub fn scratch_file(&self) -> Result<File>;         // a nameless file owned by the caller (an encoded layer, a spooled base layer, a kept stage's tar), gone with it; never kept by the store
    pub fn enter_run(&self) -> Result<RunGuard>;        // ADR-0056: held around each RUN; waits until no name is on disk (no-op when persistent)
    pub fn leave_run(&self, g: RunGuard, swept: bool) -> Result<()>; // !swept poisons the fence (no name for the build); checks the state dir's identity; makes .ranikust-building this build's again (ADR-0077)
    pub fn marker_keeper(&self) -> Result<Option<MarkerKeeper>>; // anonymous store only: MarkerKeeper::keep() creates the marker if nothing holds its name, for exits that skip close (ADR-0077)
    pub fn named_window(&self) -> Result<NamedGuard>;   // ADR-0056: a name may exist while held; waits for RUNs; fails inside one, after a failed RUN, and in a store opened on a build cut short
    pub fn scratch_name(&self) -> Result<String>;       // ANON_PREFIX + 128 random bits; removed by the next open_anonymous if left
    pub fn state_dir(&self) -> &Path;                   // canonical
    pub fn build_id(&self) -> &str;
    pub fn build_dir(&self) -> &Path;                   // builds/<id>, created by open
    pub fn get(&self, key: &EntryKey) -> Result<Option<PathBuf>>;  // a published entry; no lock
    pub fn fill<F, E>(&self, key: &EntryKey, f: F) -> Result<PathBuf>  // lock -> published? return it ->
        where F: FnOnce(&TmpEntry) -> std::result::Result<u64, E>,    //   tmp/<id>/<n> -> f (bytes written)
              E: Into<Box<dyn std::error::Error + Send + Sync>>;      //   -> fsync a blob -> rename
    pub fn cache_dir(&self, id: &str) -> Result<PathBuf>;  // caches/<sha256(id)>, created if missing
    pub fn cache_lease(&self, key: &str, sharing: CacheSharing, owner: Option<CacheOwner>,
        cancelled: &dyn Fn() -> bool) -> Result<CacheLease>;  // held for one RUN, as BuildKit's sharing=: flock + an in-process hold; a new cache made under a temporary name, owner set, renamed; an owner the user namespace does not map: Error::Rootfs(Unmapped) naming caches/<name> (ADR-0098)
    pub fn close(self) -> Result<()>;                   // removes builds/<id> and tmp/<id>; anonymous: see Invariants; Error::NotEmptied fails the build (ADR-0077)
    pub fn open_or_abandon(paths: &BuildPaths, undoing: impl FnOnce()) -> Result<Self>; // ADR-0136: open; where it fails once it took the directory as a store, undoing() then the directory emptied where abandon would (logged where it cannot be), the error returned
    pub fn abandon(self) -> Result<bool>;               // ADR-0136, before a fallback to kaniko mode on /: close, then the shared state lock turned exclusive without waiting on the same descriptor and, under it, the directory emptied where the store holds no data, whoever made it: blobs/, layers/, caches/ empty, prune_all's names only, no PRUNING_MARKER or BUILDING_MARKER; emptied with STATE_MARKER last, no PRUNING_MARKER (Error::NotAbandoned names what stays, the marker kept); true: emptied; false: another build holds it, or it holds data or a name the store does not make
    pub fn prune_all(state: &Path) -> Result<bool>;     // ranikust prune --all (ADR-0105): the caller checked state as a build's; own / refused, EX flock without waiting (Error::InUse), check_prune's checks, marker renamed PRUNING_MARKER and synced, empty by descriptor, PRUNING_MARKER last; false: was empty
    pub fn check_prune(state: &Path) -> Result<PruneCheck>; // prune_all's checks, read-only and without its lock, on state as given with no symlink followed: else prune_all's error; ranikust's prune runs it before the namespace and pins the commands it prints with it
    // prune(&PrunePolicy) -> PruneReport: [design] Phase 5, not built (ADR-0032)
}
pub struct PruneCheck { pub entries: bool, pub dev: u64, pub ino: u64, pub uid: u32 } // entries: prune_all empties it; dev, ino, uid: as stat -c %d:%i:%u prints them
pub enum CacheSharing { Shared, Locked, Private }   // shared: SH flock; locked: wait for EX; private: new dir
pub struct CacheOwner { pub uid: u32, pub gid: u32, pub mode: u32 }  // a new cache's; set-id bits dropped
impl CacheLease { pub fn path(&self) -> &Path; pub fn created(&self) -> bool }  // flock held until dropped
pub struct TmpEntry;  // file() -> Option<&File> (blob); root() -> Option<&RootPath> (layer); path() -> &Path for messages only: write through file()/root()
pub enum EntryKey { Blob(Digest), Layer(Digest) }  // Layer by chain ID, in a user namespace by chain ID and id map (rnk-snapshot, ADR-0032, ADR-0104) [open: #1]; digest() -> &Digest
pub const SANDBOX_DIR: &str = "sandbox";  // the RUN sandbox's directory at the top, made by ranikust; a name prune_all takes
pub const USERNS_PROBE_PREFIX: &str = ".ranikust-anon-userns-probe-"; // the user-namespace probe's directories, which alone stand in for the marker; the probe flocks its own while it uses it
pub struct StateDirHold;  // check_state_dir's shared lock on the state directory, held until dropped
pub enum Error { NotAStateDir { path, why }, NoFlock, Busy, InUse { path }, NotPruned { path, left }, NotAbandoned { path, left }, PruneCutShort { path }, StateNotEmpty { path, found }, UsedOnSlash, NotEmptied { path, left }, Unmarked { path, emptied, held, why }, Io, BadEntry, Cancelled, Anonymous { what, why }, Random, AnonymousFile, NameOutlivesUnlink, Rootfs, Fill }
impl Error { pub fn is_symlink_on(&self, state: &Path) -> bool }  // check_prune could not open state as given for a symlink on its path (ELOOP), not a refusal of its contents
```

`f` must return `Ok` only for a verified entry: the store does not hash a
blob again. It is synchronous; an async caller (`rnk-registry`) runs `fill`
on a blocking thread, because the lock may wait. Not hashing again rests on
one premise: no lower-trust principal writes a persistent store's state
directory. Full-sandbox `RUN`s cannot. A `Direct` `RUN` can write its own
state directory, which is therefore never a persistent store and is shared
with no other build (ADR-0077); the test
`a_kaniko_build_on_slash_leaves_nothing_its_run_wrote` holds that. Durability
(ADR-0032): a blob is `fsync`ed before its rename, a layer is not. The state
directory's checks and the `flock` on `/` belong to `ranikust` (ADR-0050).

## Invariants

Design: [locks](../../design/cache-and-store.md#locks),
[atomic publication](../../design/cache-and-store.md#atomic-publication),
[failure handling](../../design/cache-and-store.md#failure-handling).

- An entry appears only by `rename` from `tmp/`, on the same filesystem,
  after it was verified; published entries are immutable (ADR-0006, T13).
  The per-digest lock is taken before looking for the entry, so a waiter
  finds it published.
- A non-empty state directory without a regular `.ranikust-state` file is
  refused, so clean-up never runs in a directory the user pointed at by
  mistake; only the user-namespace probe's own directories
  (`USERNS_PROBE_PREFIX`, a real directory on the state directory's mount,
  told by `statx` without following) do not count: `check_marker` marks the
  directory, and `open` then removes each whose `flock` it takes (a probe
  holds its own while it uses it), through descriptors; every emptying
  (`empty_if_unused`, `prune_all`, `open_anonymous`, an anonymous `close`)
  takes the same lock, leaves one held, and keeps a store with no data
  beside it. A new state directory is `0700`; one owned by another user or
  writable by others is used with a warning. The store's directories the
  user owns take the process's gid and no setgid bit, each pinned once
  (`RootPath::set_meta_if`): the rule, its reason and its exception are in
  [atomic publication](../../design/cache-and-store.md#atomic-publication).
- The per-digest lock holds twice: an in-process lock among the store's
  threads, then `flock` among processes, because NFS, CIFS and some FUSE
  filesystems emulate `flock` per process. No `flock` support is a start-up
  error (`Error::NoFlock`). Lock files
  (`locks/{blobs,layers/v3}/<algorithm>/<hex>`, `builds/<id>.lock`) are
  never removed (but by `prune_all`, while no build holds the state
  directory), so no lock is taken on an unlinked file,
  and must be regular files, checked before they are opened for I/O (a FIFO
  would block, a device would be opened as root).
- Only the store's own kind of entry counts as published: a regular file for
  a blob, a directory for a layer, on the state directory's mount. A symlink,
  another type or a mount under an entry's name is `Error::BadEntry`, never
  followed or replaced.
- Deletion (`remove_tree`, clean-up and `close`) unlinks a symlink at the top,
  refuses a mount point there, and deletes only the directory it stat'ed: the
  opened directory's inode, device and mount must match. An upper written
  under `volatile` is never reused after a crash.
- **Anonymous store** (kaniko mode on `/`, ADR-0051, ADR-0056, ADR-0066,
  ADR-0077; design: [per state directory locks](../../design/cache-and-store.md#locks),
  [kaniko mode](../../design/kaniko-mode.md#invariants)): writes only
  `.ranikust-building` (`BUILDING_MARKER`: PID namespace and boot id), removed
  after a clean build. Blobs are anonymous files (`O_TMPFILE|O_EXCL`, memfd
  with `MFD_NOEXEC_SEAL` as fallback) named `/proc/self/fd/<n>`, refused
  unless `/proc/self/fd` is procfs; without `O_TMPFILE` a name is unlinked at
  once, refused on NFS, FUSE, SMB or AFS (`statfs`), when the file is still
  linked after the unlink and when the unlink fails
  (`Error::NameOutlivesUnlink`; a persistent store is not checked). `close`
  makes the marker this build's, closes the files, empties the state directory
  through descriptors, removes the marker only if nothing is left and the
  build was clean, and unlocks last (`Error::NotEmptied` fails the build).
  A persistent store it finds is refused, unless it holds no data: then
  it is emptied first, under the exclusive lock, as `abandon` empties it
  (ADR-0136), and the names an earlier anonymous store left with it still
  count as a build cut short (ADR-0066); the user namespace probe's
  directories, which share their prefix, never do.
- `prune_all` removes nothing a running build uses: it takes the state
  directory's `flock` exclusive without waiting, which every build's store
  holds shared for its life, and keeps it to the end, so it is refused
  while any build runs (`Error::InUse`) and a store opened meanwhile is
  `Error::Busy`. Its checks, which change nothing and all come before any
  rename, are one function `check_prune` runs too, so a caller checking
  first refuses what it would
  ([the list](../../design/cache-and-store.md#prune); each `NotAStateDir`
  but this process's own `/` and `.ranikust-building`'s
  (`Error::UsedOnSlash`)); it takes a directory holding the regular
  `.ranikust-pruning` an earlier prune left, which `open` refuses, and
  makes nothing in an empty directory. It refuses this process's own `/`
  itself (`rnk_rootfs::refuse_own_root`), whatever its caller checked. It
  empties entries in place, under their published names, so it first
  renames the marker `.ranikust-pruning` (`PRUNING_MARKER`) and syncs
  the directory, and `open` refuses a
  directory holding that name (`Error::PruneCutShort`) until a prune
  finishes: a prune killed or failed part-way never hands a build a layer
  with files missing ([why a rename](../../design/cache-and-store.md#prune)).
  It deletes through `rnk-rootfs::delete_contents` by descriptor (no
  symlink followed, no mount crossed or emptied), and only when nothing
  else is left syncs the filesystem (`syncfs`) and removes
  `.ranikust-pruning`; else `Error::NotPruned` names what is.
  `prune` by size and age (Phase 5) removes entries while builds run: per
  [ADR-0122](../../adr/0122-prune-tells-an-entry-in-use-by-a-shared-lock-and-its-last-use-by-its-markers-mtime.md),
  not built yet, a shared `flock` on each entry a build uses, and the
  entry's marker mtime as its last use.
- In kaniko mode on `/` the state directory is on the ignore list: nothing
  walks, snapshots or deletes it as part of a root. Every descriptor is
  `O_CLOEXEC`; none reaches `RUN` (T7).

## Refs

- **Design:** [`design/cache-and-store.md#store-layout`](../../design/cache-and-store.md#store-layout), [`#locks`](../../design/cache-and-store.md#locks), [`#atomic-publication`](../../design/cache-and-store.md#atomic-publication), [`#prune`](../../design/cache-and-store.md#prune), [`#ownership`](../../design/cache-and-store.md#ownership), [`#failure-handling`](../../design/cache-and-store.md#failure-handling); [`design/overlay-snapshot.md#state-directory-layout`](../../design/overlay-snapshot.md#state-directory-layout); [threat model T7, T13, T14](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0021](../../adr/0021-rnk-store-crate.md) (this crate), [ADR-0022](../../adr/0022-common-state-dir.md) (one state directory, defaults), [ADR-0006](../../adr/0006-persistent-store.md) (persistent, locks, rename, prune), [ADR-0007](../../adr/0007-run-anywhere-shared-store.md) (shared by concurrent builds), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md) (never hardlink out of the store), [ADR-0056](../../adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md) and [ADR-0077](../../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md) (the anonymous store), [ADR-0099](../../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md) (in rootless mode every operation on extracted layers runs inside the build's user namespace, and the store's directories carry mapped ids, #77), [ADR-0105](../../adr/0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md) (`prune_all`, for `prune --all`, refused when root runs it), [ADR-0104](../../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) (in rootless mode and under `--userns` extracted layers are keyed by their id map in the same state directory: `rnk-snapshot` makes the key, #77)
- **Open:** [#1](https://github.com/tran4774/ranikust/issues/1) (which digest names an extracted layer, where locks live; it must include the id map of a rootless or `--userns` build, ADR-0104; interim: layers named by the digest the caller gives, `rnk-snapshot` uses the chain ID, and in a user namespace a digest of the chain ID and the id map), [#2](https://github.com/tran4774/ranikust/issues/2) (proving the lock protocol under crashes and races), [#30](https://github.com/tran4774/ranikust/issues/30) (a cache on `/` across builds)
- **Evidence:** [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) §5 (parallel extraction 0.37 s against 0.88 s), [exp-09](../../research/experiments/09-kaniko-mode-prototype.md) finding 4 (a state directory in the walked root leaks), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) finding 9 (a hardlink farm corrupts the store)
- **kaniko:** `kaniko@44aa7ddc0 pkg/config/init.go:41` (`KanikoDir`), `:49-81` (its per-purpose directories); `pkg/util/fs_util.go:72-89` (the ignore list starts with `KanikoDir`). kaniko keeps no store between builds and takes no locks.
- **External:** <https://man7.org/linux/man-pages/man2/flock.2.html>, <https://man7.org/linux/man-pages/man2/rename.2.html>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers).

- **T0** (temp dirs): `tests/store.rs`: layout and marker, a foreign
  directory refused, `fill` publishes once, a failing `f` publishes nothing,
  concurrent `fill` from threads runs `f` once, leftovers of a dead build
  removed and a running build's kept, `close`, `cache_dir`; a directory
  holding only the probe's directories is marked and they are removed, and
  anything else with them still refused
  (`open_marks_a_directory_holding_only_ranikusts_leftovers`); another
  anonymous name, a file or a symlink with the probe's prefix is refused and
  left as it was (`only_the_probes_own_directories_stand_in_for_the_marker`);
  a probe directory whose lock is held is kept
  (`open_removes_only_the_probe_directories_no_probe_holds`), by every
  emptying too (`a_probe_directory_a_probe_holds_is_kept`,
  `an_emptying_leaves_a_probe_directory_a_probe_holds`), and one gone
  before the open reaches it skipped
  (`a_probe_directory_gone_before_it_is_opened_is_skipped`); the hold of
  `check_state_dir` keeps a prune out until dropped
  (`check_state_dir_holds_the_directory_until_dropped`); an anonymous
  store refuses a persistent store with a blob and leaves it as it was
  (`an_anonymous_store_refuses_a_persistent_store`), and empties one with
  no data, a dead build's and the sandbox's directories included, then
  opens, the anonymous names found with it making it a build cut short;
  one another build holds, or that holds `.ranikust-building` too, stays
  refused and as it was, as does one beside a name no store makes or a
  `.ranikust-pruning`
  (`an_anonymous_store_empties_a_store_with_no_data_first`); a probe
  directory left, alone or beside a store with no data, is removed and
  tells no build cut short
  (`a_probe_directory_left_tells_no_build_cut_short`). `src/store/tests.rs`:
  every directory the store makes is one that holds data or a build's
  own (`each_store_directory_holds_data_or_a_builds_own_files`).
  `tests/processes.rs`: four processes filling one digest run `f` once, a
  build killed by `SIGKILL` inside `f` leaves nothing and frees the lock,
  `prune_all` in a process whose hard `RLIMIT_NOFILE` is 256 empties a
  cache 3,000 levels deep.
  `tests/red_team_probe.rs` (Q-111): a probe directory planted with links
  out, a hard link to a file outside, a FIFO, a socket, a tree 3,000
  levels deep and directories without write or search permission is taken
  by `Store::open` without blocking or changing anything outside; a
  `prune_all` racing two builds never empties a store or a probe directory
  a build holds.
  `tests/spans.rs`. Planted symlinks as entries or build directories, a FIFO
  as a lock file, a directory as a dead build's lock file; in
  `src/store/tests.rs`, a directory swapped for a symlink or another
  directory between `statx` and open. A setgid state directory of another
  group passes neither its gid nor the bit to a directory of the store,
  and a store directory of another group takes the process's gid (the gid
  checks skipped for a user of one group), while one already the
  process's keeps its ctime; a refused `chown` is a warning only outside a
  user namespace, and the refusal itself is the error in one or where the
  namespace cannot be told, while another errno of the `chown`, a refused
  `chmod`, or `EPERM` from a step before them, is an error everywhere
  (`a_refused_group_change_is_a_warning_only_outside_a_user_namespace`); a
  symlink of another group at a store directory's name keeps its group and
  the open fails on it
  (`a_symlink_at_a_store_directory_s_name_keeps_its_group`); a symlink out
  of the state directory at each store directory's name, absolute or
  climbing, leaves what it leads to, setgid and of another group, unchanged
  through the open, a fill and a cache lease
  (`red_team_a_store_directory_planted_as_a_symlink_out_of_the_state_dir_is_never_changed`).
- **T2c** (`sysadmin`, `just test-container unit-c`):
  `a_mount_where_clean_up_deletes_is_not_emptied` (a bind mount on a dead
  build's `tmp/<id>` and on `builds/<id>` keeps its contents).
- **T1 and T2**, from `ranikust` (`setup/tests/userns.rs`, see its card):
  `Store::open` in the build's user namespace removes a dead build's
  `tmp/<id>` and `builds/<id>` holding files and a `0555` directory of
  other ids: as root of a rootless Podman container (`just
  test-rootless`), and as a regular user with subordinate ids (the VM,
  where AppArmor does not restrict user namespaces). The same for
  `prune_all` through `ranikust prune`, after a build that left such files
  in a layer, a cache mount and its own directories, with links out:
  emptied, nothing outside changed, then removed with `rmdir`.
  `setup/tests/userns/layers.rs` (#77): base layers filled and published in
  the namespace, `0555` roots included; a `SIGKILL`ed fill publishes nothing
  and the next build removes it; in T2, from a setgid state directory of a
  gid no map holds. `setup/tests/userns/layers/hostile.rs`: in the
  namespace, a store directory of another owner keeps its owner and setgid
  bit, and one that is a symlink leaves its target, root's, unchanged.
- **T0** `tests/prune.rs` (`prune_all`, the test user's own files): every
  kind of entry a build leaves emptied, marker last; refused while a build
  holds the store, nothing removed; a directory without a regular marker
  (none, a symlink, a directory) refused; a marked one, or one an earlier
  prune left, holding a file or a directory the store does not make
  refused, marker in place and nothing removed; every name the store and
  a build make taken; the machine's own `/` refused; an empty one left as it is,
  nothing made; one an anonymous store used refused; the marker renamed,
  `NotPruned`, and `open` refused while anything stays (a `0555` directory
  as a user); the rename before any removal and no file made (the order of
  inotify events); a prune cut short inside a layer, or after the last
  entry, keeps every store out (`PruneCutShort`, nothing changed) until a
  prune finishes; a `.ranikust-pruning` that is not a regular file refused
  by both until removed; `abandon` empties a store with no data, whoever
  made it, what dead builds and the sandbox left included, and leaves as
  `close` does one with an entry in `blobs/`, `layers/` or `caches/`, one
  another build holds, and one with a name the store does not make
  (`abandon_empties_a_store_with_no_data_that_no_build_holds`); what it
  cannot remove is named (`NotAbandoned`) and the marker stays, a store
  builds still open (`abandon_keeps_the_marker_while_anything_stays`); as
  `open_or_abandon` does for an open that fails once it registered its
  build, after its `undoing` lets go of the directory, a store found with
  no data (as a killed open leaves it) emptied too, one with a cache kept
  (`src/store/tests.rs`:
  `an_open_that_fails_empties_a_store_with_no_data_where_asked`);
  `check_prune`'s refusal of a path with a
  symlink on it, last or not, told apart (`is_symlink_on`) from a
  missing, unmarked or symlinked-marker one; in a cache mount, symlinks out
  (absolute, climbing), a hardlink and a FIFO, and a build directory that
  is a symlink out, removed, what they lead to unchanged.
- To do: `prune` by size and age (Phase 5); two builds as real root sharing
  one store, with a layer extracted through `rnk-rootfs::extract` (device
  nodes, foreign owners).

## Metrics

- `rnk_store.lock` (`contended`; time = lock wait), `rnk_store.fill`
  (`reused`, `bytes_written`, `bytes_reused`), `rnk_store.cleanup`,
  `rnk_store.prune` (`prune_all`, `abandon`, `open_or_abandon`, and
  `open_anonymous` where it empties a store with no data; its fields
  come with Phase 5's `prune`)
  ([rows](../../design/metrics.md#rnk-store)).
- Lock waits and reuse show what sharing the store costs and saves.

## Pitfalls

- `rename` across filesystems fails with `EXDEV`: `tmp/` lives in the state
  directory itself (cache-and-store design).
- A state directory inside the walked root leaks into the diff unless it is
  on the ignore list (exp-09 finding 4).
- Hardlinking store files into a stage root lets one `echo >>` change the
  store and every sibling (exp-12 finding 9, ADR-0017).
- An `flock` belongs to the open file description, which a forked child shares
  until `exec` closes it (`O_CLOEXEC`): while another thread forks (the `RUN`
  spawner, a test spawning processes), a dead build's lock can look held, and
  clean-up skips that build until the next start. Harmless (it only delays
  clean-up), but it made a clean-up test flaky beside a test that spawns
  processes; those live in their own test binary, and the other tests make
  FIFOs with `mknodat`, not a spawned `mkfifo`.
- Each lock gets its own descriptor: on a local filesystem two threads of one
  process exclude each other because their `flock`s are on different open file
  descriptions; elsewhere only the in-process lock does.
- `remove_tree` checks the top directory inside `rnk-store` (`statx`, then
  `sub_root`, then compare). `prune` must not reuse it for `caches/`, which
  `RUN` writes, or for anything else build content can reach, until
  `rnk-rootfs` offers a deletion that opens the top itself with `O_NOFOLLOW`
  and without crossing a mount.
- Layer entries hold image content that may be unwritable to an unprivileged
  user (a `0555` directory); deletion then fails with `EACCES`, and clean-up
  warns and leaves the build for the next start. A user other than root
  must therefore open the store inside the build's user namespace, never
  outside it.
- Deleting `builds/<id>` meets image content and may meet mounts: fd-based, by
  `mnt_id`, through `rnk-rootfs` (exp-09 D2b).
