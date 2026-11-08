//! The store: opening and clean-up, per-digest locks, publication.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use rnk_oci::Digest;
use rnk_rootfs::{FileType, IgnoreList, OpenHow, RootPath, Stat, chown_id, delete_contents};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};
use tracing::field::Empty;

use crate::anon::{self, AnonFiles};
use crate::error::{Error, Result};
use crate::fence::{Fence, NamedGuard, RunGuard};
use crate::key::EntryKey;
use crate::lock;

/// The marker file of a state directory: a regular file at its top. Its
/// own name, never the old `.ranikust-root` of a kaniko-mode workdir, so a
/// directory marked for that is never taken for a store (ADR-0050).
pub const STATE_MARKER: &str = ".ranikust-state";
use STATE_MARKER as MARKER;

/// In an anonymous store's directory while a build runs; left after a
/// build that was cut short or may have left `RUN` processes, which keeps
/// later builds in this container from making names (Q-360).
pub const BUILDING_MARKER: &str = ".ranikust-building";
use BUILDING_MARKER as BUILDING;

/// What [`STATE_MARKER`] is renamed to while [`Store::prune_all`] empties
/// the state directory, removed last. Entries are emptied in place, under
/// their published names: a prune cut short or failed leaves this file and
/// no [`STATE_MARKER`], and no store opens there until a prune finishes
/// ([`Error::PruneCutShort`]); a ranikust that predates it finds the
/// directory unmarked and refuses it too.
pub const PRUNING_MARKER: &str = ".ranikust-pruning";
use PRUNING_MARKER as PRUNING;
/// What [`BUILDING`] holds: the PID namespace and the boot of the build
/// that made it; `None` when either cannot be read, and any marker then
/// counts as this container's.
fn building_mark() -> Option<String> {
    let ns = fs::read_link("/proc/self/ns/pid").ok()?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{} {}\n", ns.to_str()?, boot.trim()))
}

/// The advice of a store opened on the traces of a build cut short
/// (ADR-0066), shared by [`CUT_SHORT`] and [`Store::named_window`]'s refusal
/// so the two say the same thing.
macro_rules! cut_short_advice {
    () => {
        "an earlier build in this container did not end cleanly and may have left RUN \
         processes; run ranikust as the container's PID 1, or restart the container"
    };
}

/// Why a store opened on the traces of a build cut short makes no name
/// (ADR-0066).
const CUT_SHORT: &str = concat!(cut_short_advice!(), ", to keep state on disk");
/// Why an anonymous store gives no path in the state directory.
const KEEPS_NOTHING: &str =
    "the store keeps nothing in the state directory a RUN could reach (ADR-0051)";
/// The directories [`Store::open`] creates in the state directory.
const DIRS: [&str; 6] = ["blobs", "layers", "caches", "locks", "tmp", "builds"];
/// The directory of the `RUN` sandbox's own files at the top of a store's
/// state directory, beside the store's own; the caller makes it, and
/// [`Store::prune_all`] counts it as the store's.
pub const SANDBOX_DIR: &str = "sandbox";
/// The start of the name of the directory rootless overlay mode's user
/// namespace probe makes at the top of a state directory before its store
/// opens: an anonymous store's name ([`crate::ANON_PREFIX`]). The probe
/// holds an exclusive `flock` on the directory itself while it uses it.
/// [`Store::open`] takes a directory holding only such directories (a
/// build cut short before its store) as a new one, and removes each it
/// can take that lock on; so does every emptying of a state directory
/// here, which leaves one a probe holds.
pub const USERNS_PROBE_PREFIX: &str = ".ranikust-anon-userns-probe-";
/// Why [`Store::open`] refuses a directory without the marker, and what to
/// do.
const UNMARKED: &str = "is not empty and has no regular file .ranikust-state; refusing to use \
                        it as the state directory (pass an empty or new --state-dir; a state \
                        directory of an earlier ranikust, marked .ranikust-root, is not reused: \
                        delete it first)";
/// Mode of the state directory and the store's own directories: image
/// content stays away from other users of the host.
const DIR_MODE: u32 = 0o700;
/// `S_ISGID`: a directory with it gives its gid, and the bit, to the
/// directories made in it.
const SETGID: u32 = 0o2000;

/// The shared `flock` on a state directory that [`Store::check_state_dir`]
/// took, held until this is dropped.
#[derive(Debug)]
#[must_use = "the state directory's lock is released when this is dropped"]
pub struct StateDirHold {
    _lock: OwnedFd,
}

/// The paths handed down to every crate of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildPaths {
    /// The build root: `/` in kaniko mode in a container, else the state
    /// directory, below which `ranikust` makes every root (ADR-0050).
    pub root: PathBuf,
    /// The state directory (`--state-dir`), which holds the store
    /// (ADR-0022).
    pub state: PathBuf,
}

impl BuildPaths {
    /// Paths from the build root and the state directory, both absolute.
    pub fn new(root: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: state.into(),
        }
    }
}

/// How a cache mount is shared: BuildKit's `sharing=` of
/// `RUN --mount=type=cache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheSharing {
    /// Every user at once.
    #[default]
    Shared,
    /// One user at a time; the others wait.
    Locked,
    /// One user at a time; another gets a new empty cache of the same key.
    Private,
}

/// The owner and mode a new cache gets: BuildKit's `mkdir` of the cache's
/// base when the mount sets `uid`, `gid` or `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheOwner {
    /// Owner; `u32::MAX` is `chown`'s "unchanged", the process's own.
    pub uid: u32,
    /// Group; `u32::MAX` as for `uid`.
    pub gid: u32,
    /// Permission bits and the sticky bit; set-id bits are dropped.
    pub mode: u32,
}

/// A cache directory held for one `RUN` ([`Store::cache_lease`]); the hold
/// ends when the lease drops, or when the process dies.
#[derive(Debug)]
pub struct CacheLease {
    path: PathBuf,
    created: bool,
    _lock: OwnedFd,
    _held: CacheHold,
}

impl CacheLease {
    /// The directory, for the `RUN`'s bind mount.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this lease made the directory (with its owner and mode).
    #[must_use]
    pub fn created(&self) -> bool {
        self.created
    }
}

/// The cache directories this process holds, apart from `flock`: on NFS,
/// CIFS and some FUSE filesystems `flock` is emulated with per-process
/// record locks, so two stages of one build would not exclude each other.
/// Per lock file: the shared holders, and whether one holds it alone.
#[derive(Debug, Default)]
struct CacheHolds(Mutex<HashMap<PathBuf, (usize, bool)>>);

impl CacheHolds {
    /// Takes `lock` in this process, without waiting; `None` when it is
    /// held in a way `exclusive` conflicts with.
    fn try_hold(self: &Arc<Self>, lock: &Path, exclusive: bool) -> Option<CacheHold> {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = map.entry(lock.to_owned()).or_insert((0, false));
        match (exclusive, *entry) {
            (_, (_, true)) | (true, (1.., _)) => return None,
            (true, (0, false)) => entry.1 = true,
            (false, (n, false)) => entry.0 = n + 1,
        }
        Some(CacheHold {
            holds: Arc::clone(self),
            lock: lock.to_owned(),
            exclusive,
        })
    }
}

/// A hold in [`CacheHolds`], given back on drop.
#[derive(Debug)]
struct CacheHold {
    holds: Arc<CacheHolds>,
    lock: PathBuf,
    exclusive: bool,
}

impl Drop for CacheHold {
    fn drop(&mut self) {
        let mut map = self.holds.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = map.get_mut(&self.lock) {
            if self.exclusive {
                entry.1 = false;
            } else {
                entry.0 = entry.0.saturating_sub(1);
            }
            if *entry == (0, false) {
                map.remove(&self.lock);
            }
        }
    }
}

/// An entry being filled, in `tmp/<build-id>/` until it is published.
///
/// A blob is a new, empty regular file opened for reading and writing; a
/// layer is a new, empty directory opened as a [`RootPath`], ready for
/// `rnk_rootfs::extract`.
#[derive(Debug)]
pub struct TmpEntry {
    path: PathBuf,
    file: Option<File>,
    root: Option<RootPath>,
}

impl TmpEntry {
    /// The entry's host path, for messages and for callers that cannot use
    /// a descriptor. Write through [`TmpEntry::file`] or
    /// [`TmpEntry::root`]: reopening this path by name resolves it again,
    /// outside the store's checks.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file of a blob entry; `None` for a layer.
    #[must_use]
    pub fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }

    /// The directory of a layer entry; `None` for a blob.
    #[must_use]
    pub fn root(&self) -> Option<&RootPath> {
        self.root.as_ref()
    }
}

/// What an anonymous store holds: its files, and the entries among them by
/// key with their size.
#[derive(Debug)]
struct Anon {
    files: AnonFiles,
    entries: Mutex<HashMap<EntryKey, (PathBuf, u64)>>,
    /// Opened on traces of a build cut short: the [`BUILDING`] marker stays
    /// at the end, so later builds keep their state in memory too.
    tainted: bool,
    /// The state directory (`O_PATH`), for the marker.
    marker_dir: OwnedFd,
    /// What this build's marker holds ([`building_mark`]).
    mark: Option<String>,
    /// A `RUN` removed or changed the marker: it stays at the end.
    tampered: AtomicBool,
    /// The exclusive `flock` on the state directory, held to the end.
    _lock: OwnedFd,
}

/// Writes an anonymous store's [`BUILDING_MARKER`] again from outside the
/// store, where the process ends before [`Store::close`] (a second signal,
/// a store still in use): the directory then keeps a marker, so no
/// persistent store opens on what a `RUN` wrote there (ADR-0077).
#[derive(Debug)]
pub struct MarkerKeeper {
    dir: OwnedFd,
    mark: Option<String>,
    state_path: PathBuf,
}

impl MarkerKeeper {
    /// Creates the marker if nothing is under its name. Anything there
    /// already, whatever it is, is left: a persistent store refuses the
    /// directory for it all the same.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the marker cannot be created.
    pub fn keep(&self) -> Result<()> {
        create_building_marker(&self.dir, &self.state_path, self.mark.as_deref()).map(drop)
    }
}

impl Anon {
    fn entry(&self, key: &EntryKey) -> Option<(PathBuf, u64)> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }
}

/// The store of one build, open in the state directory.
///
/// `Sync`: stages fill entries from several threads at once; the per-digest
/// locks keep two fills of one digest apart, in this process and in others.
#[derive(Debug)]
pub struct Store {
    state: RootPath,
    state_path: PathBuf,
    id: String,
    build_dir: PathBuf,
    /// Holds the `flock` on `builds/<id>.lock` that marks the build alive.
    /// Dropping the store only closes it: a build that did not
    /// [`Store::close`] leaves its directories to the next start. `None`
    /// for an anonymous store, which registers nothing.
    _build_lock: Option<OwnedFd>,
    /// The shared `flock` on the state directory a persistent store holds
    /// for its life, which keeps an anonymous store's exclusive one away
    /// (ADR-0077). `None` for an anonymous store, which holds the
    /// exclusive one in [`Anon`].
    _state_lock: Option<OwnedFd>,
    /// [`Store::open_anonymous`]: entries are anonymous files, and the
    /// state directory stays empty (ADR-0051).
    anon: Option<Anon>,
    /// [`Store::anonymous_file`] in a persistent store, made on first use.
    build_files: OnceLock<AnonFiles>,
    /// Keeps names and `RUN`s apart in an anonymous store; open otherwise.
    fence: Fence,
    /// The per-digest locks held by this store's threads.
    held: lock::InProcess,
    /// The cache directories held by this store's threads.
    cache_holds: Arc<CacheHolds>,
    next_tmp: AtomicU64,
}

impl Store {
    /// Opens the store in `paths.state`, creating it if needed, registers
    /// this build and removes what dead builds left in `tmp/` and
    /// `builds/`.
    ///
    /// The state directory is created (mode 0700) if missing and
    /// canonicalized; a new or empty directory gets the [`STATE_MARKER`]
    /// marker, and a non-empty one without it is refused, unless it holds
    /// only directories of the user namespace probe
    /// ([`USERNS_PROBE_PREFIX`]: each a directory on the state directory's
    /// mount, not a symlink or a mount point). Once the store is open, each
    /// such directory no probe holds is removed, through descriptors. One
    /// of the store's own directories that is there but is not a directory
    /// (a symlink, even one that stays inside, or a file) is refused. A
    /// state directory owned by another user or writable by others is used
    /// with a warning.
    /// The store's own directories take the process's gid and no setgid
    /// bit, whatever a setgid state directory or a `grpid` mount gave them,
    /// so that what the store makes carries ids a user namespace maps
    /// (ADR-0099); outside a user namespace, a filesystem that refuses the
    /// `chown` (`EPERM`) leaves them as they were, with a warning.
    ///
    /// The store holds a shared `flock` on the state directory for its
    /// life: persistent builds share it, and an anonymous store (kaniko
    /// mode on `/`), which takes it exclusively, never runs beside them.
    /// A state directory holding [`BUILDING_MARKER`] is refused: a
    /// kaniko-mode build on `/` used it and did not end cleanly, and its
    /// `RUN` may have written there what this store would reuse without
    /// hashing it again (ADR-0077). So is one holding [`PRUNING_MARKER`],
    /// whose entries a `prune --all` may have emptied part-way. These
    /// checks come before anything is written.
    ///
    /// # Errors
    ///
    /// [`Error::NotAStateDir`] for a foreign directory or a store
    /// directory that is not one, [`Error::Busy`] while an anonymous store
    /// holds it, [`Error::UsedOnSlash`] for the
    /// marker of a kaniko-mode build on `/`, [`Error::PruneCutShort`] for
    /// [`PRUNING_MARKER`], [`Error::NoFlock`] when its
    /// filesystem has no `flock`, [`Error::Io`] or [`Error::Rootfs`] when
    /// it cannot be created or opened (including
    /// [`rnk_rootfs::Error::Unsupported`] before Linux 5.8).
    pub fn open(paths: &BuildPaths) -> Result<Self> {
        Self::open_with(paths, None::<fn()>, |_| {}, |_| Ok(()))
    }

    /// As [`Store::open`], for a set-up that falls back to kaniko mode on
    /// `/` when it fails (ADR-0136): where this open fails once it took
    /// the directory as a store (marked, by it or before), it calls
    /// `undoing`, for what must let go of the directory or not be cut
    /// short first, then empties the directory where [`Store::abandon`]
    /// would, logging what it cannot, and returns the error.
    ///
    /// # Errors
    ///
    /// As [`Store::open`].
    pub fn open_or_abandon(paths: &BuildPaths, undoing: impl FnOnce()) -> Result<Self> {
        Self::open_with(paths, Some(undoing), |_| {}, |_| Ok(()))
    }

    /// [`Store::open`], with `undoing` as [`Store::open_or_abandon`] takes
    /// it, `marker_checked` run after the marker's check and before the
    /// store directories' check (where a test plants one), and `registered`
    /// once the build is registered (where a test fails the open).
    fn open_with(
        paths: &BuildPaths,
        undoing: Option<impl FnOnce()>,
        marker_checked: impl FnOnce(&RootPath),
        registered: impl FnOnce(&RootPath) -> Result<()>,
    ) -> Result<Self> {
        let host_err = |op, source| Error::Io {
            op,
            path: paths.state.clone(),
            source,
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&paths.state)
            .map_err(|e| host_err("create", e))?;
        let state_path = fs::canonicalize(&paths.state).map_err(|e| host_err("resolve", e))?;
        let state = RootPath::open(&state_path)?;
        let state_lock = state.open_at(".", &OpenHow::read().directory().no_follow())?;
        match lock::try_lock_shared(&state_lock) {
            Ok(true) => {}
            Ok(false) => return Err(Error::Busy { path: state_path }),
            Err(source) => {
                return Err(Error::NoFlock {
                    path: state_path,
                    source,
                });
            }
        }
        // In `check_state_dir`'s order, every refusal before the marker is written.
        refuse_used_on_slash(&state, &state_path)?;
        refuse_cut_short_prune(&state, &state_path)?;
        let unmarked = needs_marker(&state, &state_path)?;
        marker_checked(&state);
        check_dirs(&state, &state_path)?;
        if unmarked {
            mark(&state, &state_path)?;
        }
        let filled = (|| {
            for d in DIRS {
                state.create_dir_all(d, DIR_MODE)?;
                own_group(&state, d)?;
            }
            warn_if_shared(&state, &state_path);
            let build = register(&state, &state_path)?;
            registered(&state).map(|()| build)
        })();
        let (id, build_lock) = match filled {
            Ok(registered) => registered,
            Err(e) => {
                if let Some(undoing) = undoing {
                    undoing();
                    let abandoned = abandon_unused(&state, state_path.clone(), &state_lock);
                    log_abandoned(&state_path, abandoned);
                }
                return Err(e);
            }
        };
        let store = Self {
            build_dir: state_path.join("builds").join(&id),
            state,
            state_path,
            id,
            _build_lock: Some(build_lock),
            _state_lock: Some(state_lock),
            anon: None,
            build_files: OnceLock::new(),
            fence: Fence::default(),
            held: lock::InProcess::default(),
            cache_holds: Arc::default(),
            next_tmp: AtomicU64::new(0),
        };
        if let Err(e) = store.cleanup() {
            if let Some(undoing) = undoing {
                undoing();
                let state_path = store.state_path.clone();
                log_abandoned(&state_path, store.abandon());
            }
            return Err(e);
        }
        remove_probe_leftovers(&store.state, &store.state_path);
        Ok(store)
    }

    /// Checks, writing nothing, that [`Store::open`] would take the
    /// existing directory `state` as a state directory now: it takes its
    /// shared `flock`, and it holds [`STATE_MARKER`] or only directories of
    /// the user namespace probe ([`USERNS_PROBE_PREFIX`]), neither
    /// [`BUILDING_MARKER`] nor [`PRUNING_MARKER`], and no store directory
    /// that is not a directory. For a step that writes there before the
    /// store opens (Q-111, (a)): the lock stays held by the
    /// [`StateDirHold`] returned, which the caller drops once
    /// [`Store::open`] has returned, so that nothing that takes the
    /// directory alone (`prune --all`, an anonymous store) runs in between.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`], [`Error::NoFlock`], [`Error::NotAStateDir`],
    /// [`Error::UsedOnSlash`] and [`Error::PruneCutShort`] as
    /// [`Store::open`], [`Error::Io`] when `state` cannot be resolved or
    /// read, [`Error::Rootfs`] when it cannot be opened.
    pub fn check_state_dir(state: &Path) -> Result<StateDirHold> {
        let state_path = fs::canonicalize(state).map_err(|source| Error::Io {
            op: "resolve",
            path: state.to_owned(),
            source,
        })?;
        let root = RootPath::open(&state_path)?;
        let state_lock = root.open_at(".", &OpenHow::read().directory().no_follow())?;
        match lock::try_lock_shared(&state_lock) {
            Ok(true) => {}
            Ok(false) => return Err(Error::Busy { path: state_path }),
            Err(source) => {
                return Err(Error::NoFlock {
                    path: state_path,
                    source,
                });
            }
        }
        refuse_used_on_slash(&root, &state_path)?;
        refuse_cut_short_prune(&root, &state_path)?;
        needs_marker(&root, &state_path)?;
        check_dirs(&root, &state_path)?;
        Ok(StateDirHold { _lock: state_lock })
    }

    /// Opens a store that keeps nothing in the state directory `state`
    /// (kaniko mode on `/`, ADR-0051): every entry is an anonymous file
    /// ([`Store::anonymous_file`]) held by this store and gone with it; no
    /// lock, layout or build directory is written. `state` must exist
    /// (`ranikust` checks and creates it). Its anonymous files are named
    /// `/proc/self/fd/<n>` to the caller's code, which only this process
    /// resolves.
    ///
    /// Only [`BUILDING_MARKER`] is written, holding this container's PID
    /// namespace and boot, and removed by a clean [`Store::close`] (ADR-0056):
    /// a store opened on this container's marker, or on names an earlier
    /// store left ([`crate::ANON_PREFIX`], removed here), makes no name for
    /// the build and keeps the marker (ADR-0066). The user namespace
    /// probe's directories ([`USERNS_PROBE_PREFIX`]) share that prefix and
    /// are removed too, each only under its own lock (one a probe holds
    /// stays, here and at [`Store::close`], and keeps a store with no data
    /// below), but made before any store opens they tell no build cut
    /// short. A marker from another container or boot is taken over.
    ///
    /// The caller must have given the process no more authority than its
    /// `RUN`s (`rnk_sandbox::trim_to_run_capabilities`, ADR-0078) before
    /// this: the store later deletes, as this process, names a `RUN` can
    /// reach (its marker, what a `RUN` left), safe only on that condition.
    ///
    /// Anything else in the state directory is refused, a persistent store
    /// first: a `Direct` `RUN` writes the state directory as root, and the
    /// builds of a persistent store reuse its entries without hashing them
    /// again (ADR-0077). A store with no data, nothing in `blobs/`,
    /// `layers/` or `caches/` (as a build killed in a set-up that falls
    /// back here leaves it, ADR-0136), has no entry to reuse: it is emptied
    /// first, under the lock, as [`Store::abandon`] does. The store holds
    /// an exclusive `flock` on the state directory for its life, which a
    /// persistent store's shared one excludes, and [`Store::close`]
    /// empties the directory of whatever a `RUN` left there.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] while another build holds the state directory,
    /// [`Error::StateNotEmpty`] for anything but this store's own marker
    /// and leftovers in it, unless it is a store with no data, which is
    /// emptied first as [`Store::abandon`] does ([`Error::NotAbandoned`]
    /// for what stays, [`Error::Rootfs`] where `state` is this process's
    /// own `/`), [`Error::Io`] when `state` cannot be resolved, opened or
    /// synced, or `/proc/self/fd` is missing; [`Error::Rootfs`] as
    /// [`Store::open`].
    pub fn open_anonymous(state: &Path) -> Result<Self> {
        let host_err = |op, source| Error::Io {
            op,
            path: state.to_owned(),
            source,
        };
        let state_path = fs::canonicalize(state).map_err(|e| host_err("resolve", e))?;
        let root = RootPath::open(&state_path)?;
        // Both descriptors come from the one directory `root` holds, never
        // from the path again, which a `RUN` of an earlier build could have
        // pointed elsewhere in between.
        let dir = root.open_at(".", &OpenHow::path().directory())?;
        check_proc_fd()?;
        // One build at a time per state directory, held for the store's
        // life: a second one (another container on the same volume) would
        // see this one's names and take its marker for a dead build's, and
        // a persistent store's shared lock keeps it off a store (ADR-0077).
        let lock = root.open_at(".", &OpenHow::read().directory())?;
        lock_state_dir(&lock, &state_path)?;
        // Nothing but this store's own marker and leftovers, checked under
        // the lock and before anything is removed or written: a persistent
        // store's entries would be in a `Direct` `RUN`'s reach (ADR-0077).
        let own = |n: &OsString| n == BUILDING || anon::is_leftover(n);
        let names = list(&root, Path::new("."))?;
        let found_leftovers = names.iter().any(|n| anon::left_by_a_store(n));
        let foreign: Vec<OsString> = names.into_iter().filter(|n| !own(n)).collect();
        // A store with no data, such as a build killed in a set-up that
        // falls back here leaves (ADR-0136), holds nothing a `RUN` could
        // change for a later build: emptied, under this lock.
        if !foreign.is_empty() && !empty_if_unused(&root, &state_path, &lock)? {
            return Err(Error::StateNotEmpty {
                path: state_path,
                found: describe_foreign(&foreign),
            });
        }
        // A volume outlives the container: what a `RUN` writes there is
        // emptied at the end, but a build that ends before then leaves it,
        // and a `RUN` can end the build (ADR-0077).
        // On another mount than `/` but not its root, the volume is mounted
        // at a parent: a `RUN` can rename the state directory and leave a
        // look-alike, unmarked, at its path.
        let slash = RootPath::open("/")?;
        if root.statx("/")?.mount_root {
            tracing::warn!(
                target: "rnk_store",
                "state directory {} is a volume: kaniko mode on / keeps nothing there and empties \
                 it at the end, but a RUN can write it, and a build that ends early leaves what \
                 the RUN wrote; never give this volume to overlay-mode or per-stage builds \
                 (ADR-0077)",
                state_path.display()
            );
        } else if root.mnt_id() != slash.mnt_id() {
            tracing::warn!(
                target: "rnk_store",
                "state directory {} is a directory of a volume mounted at one of its parents: a \
                 RUN can write it, and can rename it and leave a look-alike at its path that no \
                 marker covers; mount the volume at --state-dir itself, and never give this \
                 volume to overlay-mode or per-stage builds (ADR-0077)",
                state_path.display()
            );
        }
        // An earlier store's names tell a build cut short (ADR-0066), even
        // where the emptying above removed them.
        let leftovers = anon::remove_leftovers(&root, &state_path)? || found_leftovers;
        // A build that did not end cleanly here may have left `RUN`
        // processes no sweep reached (ranikust killed mid-`RUN`, not PID 1):
        // they would see a name, so this build makes none (ADR-0066).
        let mark = building_mark();
        let marker = read_building_marker(&root, &state_path, mark.as_deref())?;
        let interrupted = match marker {
            Marker::Ours => true,
            Marker::Theirs => false,
            // Names with no marker: it was deleted (a `RUN` can), or made
            // by an older ranikust.
            Marker::Absent => leftovers,
        };
        // Written unless this container's marker is kept: made anew when
        // names were left with none, so later builds heed it, and replaced
        // by a fresh file when what is kept is not a regular file holding
        // this container's mark (a directory a `RUN` filled, say).
        let marker_dir = dir.try_clone().map_err(|e| host_err("duplicate", e))?;
        match marker {
            Marker::Ours => {
                ensure_own_marker(&root, &marker_dir, &state_path, mark.as_deref())?;
            }
            // A stale marker (a dead build's, the directory being locked) is
            // replaced by a new file, never written through.
            Marker::Theirs => {
                root.remove_file(BUILDING)?;
                write_building_marker(&root, &state_path, mark.as_deref())?;
            }
            Marker::Absent => write_building_marker(&root, &state_path, mark.as_deref())?,
        }
        let tainted = interrupted.then_some(CUT_SHORT);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let id = format!("{:x}-{nanos:x}", std::process::id());
        let fence = Fence::closed();
        let files = AnonFiles::new(dir, &state_path, fence.clone());
        if let Some(why) = tainted {
            files.tainted(why);
        }
        Ok(Self {
            build_dir: state_path.join("builds").join(&id),
            anon: Some(Anon {
                files,
                entries: Mutex::default(),
                tainted: tainted.is_some(),
                marker_dir,
                mark,
                tampered: AtomicBool::new(false),
                _lock: lock,
            }),
            state: root,
            state_path,
            id,
            _build_lock: None,
            _state_lock: None,
            build_files: OnceLock::new(),
            fence,
            held: lock::InProcess::default(),
            cache_holds: Arc::default(),
            next_tmp: AtomicU64::new(0),
        })
    }

    /// Whether this store keeps its entries as anonymous files
    /// ([`Store::open_anonymous`]).
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.anon.is_some()
    }

    /// Whether the anonymous files made now are `memfd`s, charged to the
    /// container's memory, rather than files on disk: the state directory
    /// takes no file, or no file may be made by name for the rest of the
    /// build. It can turn true during a build, never back. A caller that
    /// would keep a file for long can then keep less.
    #[must_use]
    pub fn files_in_memory(&self) -> bool {
        match &self.anon {
            Some(anon) => anon.files.in_memory(),
            None => self.build_files.get().is_some_and(AnonFiles::in_memory),
        }
    }

    /// A new, empty file open for reading and writing that no other process
    /// can reach by a path, kept open as long as the store, and its name
    /// for this process's own code (`/proc/self/fd/<n>`): for what a build
    /// writes and reads back for the whole build.
    /// In an anonymous store it lives in the state directory (`O_TMPFILE`)
    /// or in memory (`memfd`); in a persistent one, in the build directory.
    ///
    /// # Errors
    ///
    /// [`Error::AnonymousFile`] when neither kind of file can be made.
    pub fn anonymous_file(&self) -> Result<(File, PathBuf)> {
        let files = self.anonymous_files()?;
        let file = files.create()?;
        let kept = file.try_clone().map_err(|e| Error::AnonymousFile {
            path: self.state_path.clone(),
            source: e,
        })?;
        Ok((file, files.keep(kept)))
    }

    /// A new anonymous file like [`Store::anonymous_file`], but owned by the
    /// caller: its disk space or memory goes with the file's last
    /// descriptor, not with the store. Name it to this process with
    /// `/proc/self/fd/<n>` of the file's descriptor. For what a build keeps
    /// for a while and then drops, such as an encoded layer released once
    /// pushed (ADR-0055) or a kept stage's tar.
    ///
    /// # Errors
    ///
    /// As [`Store::anonymous_file`].
    pub fn scratch_file(&self) -> Result<File> {
        // Not kept by the store: gone with the caller's file.
        self.anonymous_files()?.create()
    }

    /// Counts a `RUN` in: in an anonymous store, waits until no named file
    /// or unpacked directory of the build is on disk, and keeps any from
    /// being made until the guard drops. Drop it once the `RUN`'s process
    /// tree is gone. In a persistent store it does nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Anonymous`] when named state outlives the wait: a bug in the
    /// order of work, not a race.
    pub fn enter_run(&self) -> Result<RunGuard> {
        self.fence.enter_run()
    }

    /// Counts the `RUN` of `guard` out. `swept` says whether `Sandbox::run`
    /// returned without an error, its process tree gone; otherwise no
    /// name is made for the rest of the build (its processes may live). In
    /// an anonymous store the state directory must still be the one opened
    /// (a `RUN` may have moved it, or made `/var/lib` a link): checked by
    /// device, inode and mount, without following a link. Its marker must
    /// still be the one this build wrote: a `RUN` that removed or changed it
    /// gets it written again, through the directory held, and the build
    /// then makes no more names and keeps the marker at its end (ADR-0077).
    ///
    /// # Errors
    ///
    /// [`Error::BadEntry`] when the state directory changed; the fence is
    /// then closed to names too. [`Error::Io`] or [`Error::Rootfs`] when
    /// the marker cannot be written again.
    pub fn leave_run(&self, guard: RunGuard, swept: bool) -> Result<()> {
        if !swept {
            self.fence.poison();
        }
        let checked = match &self.anon {
            Some(anon) => {
                let same = self.same_state_dir();
                let marked = self.keep_own_marker(anon);
                same.and(marked)
            }
            None => Ok(()),
        };
        if checked.is_err() {
            self.fence.poison();
        }
        drop(guard);
        checked
    }

    /// [`ensure_own_marker`] for an anonymous store; a marker not this
    /// build's poisons the fence and stays at the end.
    fn keep_own_marker(&self, anon: &Anon) -> Result<()> {
        let remark = || {
            ensure_own_marker(
                &self.state,
                &anon.marker_dir,
                &self.state_path,
                anon.mark.as_deref(),
            )
        };
        // A `RUN` that filled the filesystem can keep the marker from being
        // made; no `RUN` runs now, so a forged `.ranikust-state` goes, which
        // frees an inode, and it is tried once more.
        let replaced = match remark() {
            Ok(replaced) => replaced,
            Err(_) => {
                remove_state_marker(&self.state);
                remark()?
            }
        };
        if replaced {
            anon.tampered.store(true, Ordering::Relaxed);
            self.fence.poison();
            tracing::warn!(
                target: "rnk_store",
                "state directory {}: a RUN removed or changed {BUILDING}; it is written again and \
                 stays at the end, and this build makes no more names (ADR-0077)",
                self.state_path.display()
            );
        }
        Ok(())
    }

    /// The [`MarkerKeeper`] of an anonymous store; `None` for a persistent
    /// one.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the state directory's descriptor cannot be
    /// duplicated.
    pub fn marker_keeper(&self) -> Result<Option<MarkerKeeper>> {
        let Some(anon) = &self.anon else {
            return Ok(None);
        };
        let dir = anon.marker_dir.try_clone().map_err(|source| Error::Io {
            op: "duplicate",
            path: self.state_path.clone(),
            source,
        })?;
        Ok(Some(MarkerKeeper {
            dir,
            mark: anon.mark.clone(),
            state_path: self.state_path.clone(),
        }))
    }

    /// Whether the state directory's path still leads, with no link, to the
    /// directory this store holds.
    fn same_state_dir(&self) -> Result<()> {
        let changed = || Error::BadEntry {
            path: self.state_path.clone(),
            reason: "the state directory changed during a RUN (moved, or a link put on its \
                     path): what is now at that path is not the directory this build marked, so \
                     discard that directory or volume; never give it to another build",
        };
        let now = RootPath::open(&self.state_path).map_err(|_| changed())?;
        let (a, b) = (now.statx("/")?, self.state.statx("/")?);
        let same = a.ino == b.ino
            && (a.dev_major, a.dev_minor) == (b.dev_major, b.dev_minor)
            && now.mnt_id() == self.state.mnt_id();
        if same { Ok(()) } else { Err(changed()) }
    }

    /// A window in which named state may exist on disk, for a directory the
    /// caller unpacks in the state directory and deletes before it drops
    /// the guard (an image or a kept stage read by `COPY --from`): in an
    /// anonymous store, waits until no `RUN` runs and keeps any from
    /// starting until the guard drops. The directory's name comes from
    /// [`Store::scratch_name`]. In a persistent store it does nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Anonymous`] when asked from inside a `RUN` on this thread,
    /// after a `RUN` that ended with an error, or in a store opened on the
    /// traces of a build cut short, whose `RUN` processes may live on
    /// outside any fence (ADR-0066).
    pub fn named_window(&self) -> Result<NamedGuard> {
        if self.anon.as_ref().is_some_and(|a| a.tainted) {
            return Err(Error::Anonymous {
                what: "COPY --from a kept stage or an image, which unpacks it by name",
                why: concat!(cut_short_advice!(), " (ADR-0066)"),
            });
        }
        self.fence.named()
    }

    /// A fresh name in the state directory for named state an anonymous
    /// store's next start removes if this build leaves it (a crash):
    /// [`crate::ANON_PREFIX`] and 128 random bits.
    ///
    /// # Errors
    ///
    /// [`Error::Random`] when the kernel has no randomness to give.
    pub fn scratch_name(&self) -> Result<String> {
        anon::random_name()
    }

    /// Where this store makes its anonymous files: the state directory of an
    /// anonymous store, the build directory of a persistent one.
    fn anonymous_files(&self) -> Result<&AnonFiles> {
        match &self.anon {
            Some(anon) => Ok(&anon.files),
            None => self.persistent_files(),
        }
    }

    /// The anonymous files of a persistent store, in its build directory:
    /// a store-wide set made on first use.
    fn persistent_files(&self) -> Result<&AnonFiles> {
        if let Some(files) = self.build_files.get() {
            return Ok(files);
        }
        // Through the state directory's own descriptor, never the build
        // directory's path again.
        let rel = Path::new("builds").join(&self.id);
        let dir = self
            .state
            .open_at(&rel, &OpenHow::path().directory().no_follow())?;
        Ok(self
            .build_files
            .get_or_init(|| AnonFiles::new(dir, &self.build_dir, Fence::default())))
    }

    /// The canonical state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_path
    }

    /// This build's id, unique among the builds sharing the store.
    #[must_use]
    pub fn build_id(&self) -> &str {
        &self.id
    }

    /// `builds/<build-id>`, private to this build (stage roots, uppers);
    /// removed by [`Store::close`], or by the next start after a crash.
    #[must_use]
    pub fn build_dir(&self) -> &Path {
        &self.build_dir
    }

    /// The published entry `key`, if any. Published entries are complete
    /// and never change, so no lock is needed to read them.
    ///
    /// # Errors
    ///
    /// [`Error::BadEntry`] when something else than the entry sits under
    /// its name (a symlink, the wrong type, a mount); [`Error::Rootfs`]
    /// when it cannot be looked up.
    pub fn get(&self, key: &EntryKey) -> Result<Option<PathBuf>> {
        if let Some(anon) = &self.anon {
            return Ok(anon.entry(key).map(|(path, _)| path));
        }
        Ok(self.published(key)?.map(|_| self.host(&key.path())))
    }

    /// Publishes the entry `key`, filled by `f`, unless it is published
    /// already; returns its host path.
    ///
    /// Takes the per-digest lock first, so a second build or thread asking
    /// for the same digest waits and then finds the entry published, and
    /// `f` runs at most once per digest. `f` fills a new [`TmpEntry`] and
    /// returns the number of bytes it wrote; it must return `Ok` only for a
    /// verified entry (a blob whose digest matched, a layer extracted
    /// completely). A blob is then `fsync`ed (an extracted layer is not; ADR-0032) and the entry
    /// renamed into place.
    ///
    /// # Errors
    ///
    /// [`Error::Fill`] when `f` fails, after its temporary entry is removed;
    /// nothing is published. [`Error::BadEntry`] as [`Store::get`].
    /// [`Error::Rootfs`] or [`Error::Io`] when the store cannot lock,
    /// create or rename.
    pub fn fill<F, E>(&self, key: &EntryKey, f: F) -> Result<PathBuf>
    where
        F: FnOnce(&TmpEntry) -> std::result::Result<u64, E>,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let span = tracing::info_span!(
            "rnk_store.fill",
            reused = Empty,
            bytes_written = Empty,
            bytes_reused = Empty,
        );
        let _entered = span.enter();
        if let Some(anon) = &self.anon {
            return self.fill_anonymous(anon, key, f, &span);
        }
        let _lock = self.lock(key)?;
        let rel = key.path();
        if let Some(size) = self.published(key)? {
            record_reused(&span, size);
            return Ok(self.host(&rel));
        }
        self.state.create_dir_all(key.parent(), DIR_MODE)?;
        let n = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        let tmp = PathBuf::from("tmp").join(&self.id).join(n.to_string());
        let entry = self.create_tmp(key, &tmp)?;
        let filled = f(&entry);
        let published = match filled {
            Ok(bytes) => self.publish(entry, &tmp, &rel).map(|()| bytes),
            Err(e) => {
                drop(entry);
                self.discard(&tmp);
                return Err(Error::Fill {
                    key: key.to_string(),
                    source: e.into(),
                });
            }
        };
        match published {
            Ok(bytes) => {
                record_written(&span, bytes);
                Ok(self.host(&rel))
            }
            Err(e) => {
                self.discard(&tmp);
                Err(e)
            }
        }
    }

    /// [`Store::fill`] in an anonymous store: blobs only, each an anonymous
    /// file; one fill per key at a time among this store's threads.
    fn fill_anonymous<F, E>(
        &self,
        anon: &Anon,
        key: &EntryKey,
        f: F,
        span: &tracing::Span,
    ) -> Result<PathBuf>
    where
        F: FnOnce(&TmpEntry) -> std::result::Result<u64, E>,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        if matches!(key, EntryKey::Layer(_)) {
            return Err(Error::Anonymous {
                what: "an extracted layer in the store",
                why: KEEPS_NOTHING,
            });
        }
        let (_held, _) = self.held.acquire(&key.lock_path());
        if let Some((path, size)) = anon.entry(key) {
            record_reused(span, size);
            return Ok(path);
        }
        let file = anon.files.create()?;
        let entry = TmpEntry {
            path: PathBuf::from("anonymous file"),
            file: Some(file),
            root: None,
        };
        let bytes = f(&entry).map_err(|e| Error::Fill {
            key: key.to_string(),
            source: e.into(),
        })?;
        let Some(file) = entry.file else {
            return Err(Error::Anonymous {
                what: "a blob whose filler took its file",
                why: "an anonymous store keeps a blob only as its open file (ADR-0051)",
            });
        };
        let path = anon.files.keep(file);
        anon.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), (path.clone(), bytes));
        record_written(span, bytes);
        Ok(path)
    }

    /// `caches/<sha256(id)>`, the directory of a `RUN --mount=type=cache`
    /// with this id, created if missing and kept between builds. Hashing
    /// keeps any id, `../..` included, a single plain name.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when it cannot be created;
    /// [`Error::Anonymous`] in an anonymous store.
    pub fn cache_dir(&self, id: &str) -> Result<PathBuf> {
        if self.anon.is_some() {
            return Err(Error::Anonymous {
                what: "RUN --mount=type=cache",
                why: KEEPS_NOTHING,
            });
        }
        let hash: [u8; 32] = Sha256::digest(id.as_bytes()).into();
        let rel = Path::new("caches").join(Digest::from_sha256_bytes(&hash).encoded());
        self.state.create_dir_all(&rel, 0o755)?;
        Ok(self.host(&rel))
    }

    /// A directory of the `RUN --mount=type=cache` key `key`, held for one
    /// `RUN` as `sharing` says, as BuildKit's cache mounts
    /// (`solver/llbsolver/mounts/mount.go` `getRefCacheDir`):
    ///
    /// - [`CacheSharing::Shared`]: the first directory of the key no build
    ///   holds alone, used by every shared holder at once;
    /// - [`CacheSharing::Locked`]: a directory no one else holds, waiting
    ///   (and polling `cancelled` every 100 ms) while every one is held;
    /// - [`CacheSharing::Private`]: a directory no one else holds, or a new
    ///   empty one of the same key when every one is held.
    ///
    /// The first directory of a key is `caches/<sha256(key)>` (as
    /// [`Store::cache_dir`]), the next `caches/<sha256(key)>-<n>`. A lease
    /// is an `flock` on `locks/caches/<name>.lock`, shared or exclusive, and
    /// a hold in this process (for filesystems whose `flock` is per
    /// process), kept until the lease drops; the kernel releases the `flock`
    /// if the process dies. A new directory is made under a temporary name,
    /// given `owner` (else root's, `0755`) through its descriptor and renamed
    /// into place, all under the key's pool lock: a failure never leaves a
    /// cache with the wrong owner. A caller taking several leases for one
    /// `RUN` takes them in the order of their keys.
    ///
    /// # Errors
    ///
    /// [`Error::Cancelled`] when `cancelled` turns true while waiting;
    /// [`Error::Rootfs`] or [`Error::Io`] when a directory or lock file
    /// cannot be made or locked; [`Error::BadEntry`] when one is not what
    /// the store made.
    pub fn cache_lease(
        &self,
        key: &str,
        sharing: CacheSharing,
        owner: Option<CacheOwner>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CacheLease> {
        if self.anon.is_some() {
            return Err(Error::Anonymous {
                what: "RUN --mount=type=cache",
                why: KEEPS_NOTHING,
            });
        }
        let hash: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        let base = Digest::from_sha256_bytes(&hash).encoded().to_owned();
        let locks = Path::new("locks").join("caches");
        self.state.create_dir_all(&locks, DIR_MODE)?;
        let name = |n: usize| {
            if n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        };
        let exclusive = sharing != CacheSharing::Shared;
        loop {
            // The pool lock orders the scan and the creation of a new
            // directory between builds; it is held only for that.
            // In this process too: where flock is per process (NFS,
            // CIFS), two stages of one build would both hold it.
            let pool_rel = locks.join(format!("{base}.pool"));
            let (pool_held, _) = self.held.acquire(&pool_rel);
            let pool = lock::open(&self.state, &pool_rel)?;
            lock::lock(&pool).map_err(|e| self.io_err("flock", &locks, e))?;
            let mut n = 0;
            let mut any = false;
            while self.cache_entry_exists(&name(n))? {
                any = true;
                if let Some(lease) = self.try_cache(&locks, &name(n), exclusive, false)? {
                    return Ok(lease);
                }
                n += 1;
            }
            if sharing == CacheSharing::Locked && any {
                drop((pool, pool_held));
                if cancelled() {
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            let Some(lease) = self.try_cache(&locks, &name(n), exclusive, true)? else {
                // A lock file kept by a cache removed under it, still held:
                // wait for its holder as for a locked cache.
                drop((pool, pool_held));
                if cancelled() {
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            };
            self.make_cache(&name(n), owner)?;
            return Ok(lease);
        }
    }

    /// Takes `caches/<name>`'s lock without waiting, in this process and
    /// with `flock`: `None` when it is held.
    fn try_cache(
        &self,
        locks: &Path,
        name: &str,
        exclusive: bool,
        created: bool,
    ) -> Result<Option<CacheLease>> {
        let lock_rel = locks.join(format!("{name}.lock"));
        let Some(held) = self.cache_holds.try_hold(&lock_rel, exclusive) else {
            return Ok(None);
        };
        let fd = lock::open(&self.state, &lock_rel)?;
        let got = if exclusive {
            lock::try_lock(&fd)
        } else {
            lock::try_lock_shared(&fd)
        }
        .map_err(|e| self.io_err("flock", locks, e))?;
        if !got {
            return Ok(None);
        }
        Ok(Some(CacheLease {
            path: self.host(&Path::new("caches").join(name)),
            created,
            _lock: fd,
            _held: held,
        }))
    }

    /// Makes `caches/<name>`: a new directory under a temporary name, its
    /// owner and mode set through its descriptor, then renamed into place.
    fn make_cache(&self, name: &str, owner: Option<CacheOwner>) -> Result<()> {
        let tmp = Path::new("caches").join(format!(".{name}.new-{}", self.id));
        if self.state.statx(&tmp).is_ok() {
            remove_tree(&self.state, &tmp)?;
        }
        self.state.create_dir_all(&tmp, 0o700)?;
        let made = self.fill_cache(&tmp, name, owner);
        if made.is_err() {
            // Whatever failed, the temporary directory goes; the error to
            // report is the first one.
            if let Err(e) = remove_tree(&self.state, &tmp) {
                tracing::warn!("rnk-store: cannot remove {}: {e}", tmp.display());
            }
        }
        made
    }

    /// Gives the new cache `tmp` its owner and mode through its descriptor,
    /// then renames it to `caches/<name>`, never over an existing entry.
    fn fill_cache(&self, tmp: &Path, name: &str, owner: Option<CacheOwner>) -> Result<()> {
        let fd = self
            .state
            .open_at(tmp, &OpenHow::read().directory().no_follow())?;
        let st = rustix::fs::fstat(&fd).map_err(|e| self.io_err("stat", tmp, e.into()))?;
        // Without one, the process's own ids: root's in a build.
        let owner = owner.unwrap_or(CacheOwner {
            uid: st.st_uid,
            gid: st.st_gid,
            mode: 0o755,
        });
        if (st.st_uid, st.st_gid) != (owner.uid, owner.gid) {
            rustix::fs::fchown(
                &fd,
                chown_id(owner.uid).map(rustix::fs::Uid::from_raw),
                chown_id(owner.gid).map(rustix::fs::Gid::from_raw),
            )
            .map_err(|e| match e {
                // An owner the user namespace does not map (ADR-0098).
                rustix::io::Errno::INVAL => {
                    Error::Rootfs(rnk_rootfs::Error::Unmapped(rnk_rootfs::Unmapped::chown(
                        &Path::new("caches").join(name),
                        Some(owner.uid),
                        Some(owner.gid),
                    )))
                }
                e => self.io_err("chown", tmp, e.into()),
            })?;
        }
        rustix::fs::fchmod(&fd, rustix::fs::Mode::from_raw_mode(owner.mode & 0o1777))
            .map_err(|e| self.io_err("chmod", tmp, e.into()))?;
        drop(fd);
        self.state
            .rename_noreplace(tmp, Path::new("caches").join(name))?;
        Ok(())
    }

    /// Whether `caches/<name>` exists, as a directory of the state
    /// directory's mount; anything else there is refused.
    fn cache_entry_exists(&self, name: &str) -> Result<bool> {
        let rel = Path::new("caches").join(name);
        match self.state.statx(&rel) {
            Ok(st)
                if st.file_type() == FileType::Directory
                    && !st.mount_root
                    && st.mnt_id == self.state.mnt_id() =>
            {
                Ok(true)
            }
            Ok(_) => Err(Error::BadEntry {
                path: rel,
                reason: "a cache must be a directory on the state directory's mount",
            }),
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Ends the build: removes `builds/<build-id>` and `tmp/<build-id>`,
    /// then releases the build's lock.
    ///
    /// An anonymous store ends in this order (ADR-0077):
    ///
    /// 1. its marker is made this build's again, first, so that whatever
    ///    happens next the directory keeps one: anything else under its
    ///    name (removed, another type, another build's text) is replaced,
    ///    and then stays at the end;
    /// 2. its files go with their descriptors (on a filesystem that keeps
    ///    a name for an open removed file, the name goes with them);
    /// 3. the state directory is emptied of whatever a `RUN` wrote there,
    ///    through descriptors, never following a link or crossing a mount,
    ///    a forged `.ranikust-state` first; a marker step 1 could not make
    ///    (a filesystem a `RUN` filled) is tried again right after that
    ///    file goes and once more after the emptying, and then stays;
    /// 4. the marker goes only when nothing else is left and the build was
    ///    clean: no cut-short start, no `RUN` that may have left processes,
    ///    no marker a `RUN` changed;
    /// 5. the lock on the state directory goes last.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] or [`Error::BadEntry`] when a directory cannot be
    /// removed, for example because a mount is still inside it or on it;
    /// the next start retries. For an anonymous store, [`Error::NotEmptied`]
    /// when the state directory keeps anything but its marker and the user
    /// namespace probe's directories a probe holds, and
    /// [`Error::Unmarked`] when no marker can be written even after the
    /// emptying: the caller must fail the build, since that directory must
    /// not serve another one.
    pub fn close(self) -> Result<()> {
        let Some(anon) = self.anon else {
            remove_build(&self.state, &self.id)?;
            return Ok(());
        };
        let Anon {
            files,
            entries,
            tainted,
            marker_dir,
            mark,
            tampered,
            _lock: lock,
        } = anon;
        let remark =
            || ensure_own_marker(&self.state, &marker_dir, &self.state_path, mark.as_deref());
        let first = remark();
        drop(entries);
        drop(files);
        // A `RUN` can keep the marker from being made (a filesystem it
        // filled, whose inodes root has no reserve of). A forged
        // `.ranikust-state` then goes first, which frees an inode and
        // leaves a directory no persistent store opens, and the marker is
        // tried again before the rest is emptied, which can take long; and
        // once more after, when that space is free. A marker made only then
        // stays, as one a `RUN` removed does.
        let early = match first {
            Ok(replaced) => Some(replaced),
            Err(e) => {
                remove_state_marker(&self.state);
                tracing::warn!(
                    target: "rnk_store",
                    "state directory {}: {BUILDING} cannot be made this build's ({e}); a RUN may \
                     have filled the filesystem, so it is tried again while the directory is \
                     emptied (ADR-0077)",
                    self.state_path.display()
                );
                remark().ok().map(|_| true)
            }
        };
        let emptied = empty_state_dir(&self.state, &self.state_path);
        let replaced = match early {
            Some(replaced) => replaced,
            None => match remark() {
                Ok(_) => true,
                Err(e) => {
                    drop(lock);
                    return Err(Error::Unmarked {
                        path: self.state_path.clone(),
                        emptied: emptied.is_ok(),
                        // ranikust's own marker, made empty when its text
                        // could not be written, is not what a `RUN` made.
                        held: match marker_text(&self.state, &self.state_path, 1) {
                            Ok(None) => false,
                            Ok(Some(Some(h))) => !h.is_empty(),
                            _ => true,
                        },
                        why: e.to_string(),
                    });
                }
            },
        };
        let keep = emptied.is_err()
            || replaced
            || tainted
            || tampered.load(Ordering::Relaxed)
            || self.fence.is_poisoned();
        if !keep {
            match self.state.remove_file(BUILDING) {
                Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {}
                r => r?,
            }
        }
        drop(lock);
        emptied
    }

    /// Closes a store no build used, as [`Store::close`] does, then empties
    /// the state directory where it holds no data, whoever made it a store:
    /// for a set-up that fails before its build and falls back to kaniko
    /// mode on `/` (ADR-0136). A set-up killed before it got here leaves
    /// such a store, which the next one that fails empties, as kaniko mode
    /// on `/` does ([`Store::open_anonymous`]). The store's shared `flock`
    /// is turned exclusive without waiting, on the same descriptor
    /// (`flock(2)` drops the shared lock first, and a refused attempt
    /// leaves none), and the directory is checked under it: while another build holds it, nothing
    /// is emptied, and none adds data meanwhile. It holds no data where
    /// `blobs/`, `layers/` and `caches/` hold nothing, its other names are
    /// those [`Store::prune_all`] takes (the store's, the sandbox's, probe
    /// leftovers; what dead builds left in `builds/`, `tmp/` and `locks/`),
    /// and it holds neither [`PRUNING_MARKER`] nor [`BUILDING_MARKER`].
    /// Then it is emptied as `prune_all` does, but with [`STATE_MARKER`]
    /// kept to the end in place of a [`PRUNING_MARKER`]: no entry a later
    /// build reuses can be left half-deleted, so an emptying cut short
    /// leaves a store with no data, which the next open takes. `false` when
    /// nothing was emptied: an anonymous store, one another build holds,
    /// one that holds data, a name the store does not make,
    /// [`PRUNING_MARKER`], [`BUILDING_MARKER`], or a directory of the user
    /// namespace probe that a probe holds.
    ///
    /// # Errors
    ///
    /// As [`Store::close`]; once the lock is taken, [`Error::Io`] or
    /// [`Error::Rootfs`] when the directory, `blobs/`, `layers/` or
    /// `caches/` cannot be read, [`Error::Io`] when the filesystem cannot
    /// be synced, [`Error::Rootfs`] for this process's own `/`, and
    /// [`Error::NotAbandoned`] where anything but [`STATE_MARKER`] stays,
    /// which then stays too; [`Error::NoFlock`] when it cannot be taken.
    pub fn abandon(self) -> Result<bool> {
        if self.anon.is_some() {
            self.close()?;
            return Ok(false);
        }
        let Self {
            state,
            state_path,
            id,
            _build_lock: build_lock,
            _state_lock: state_lock,
            ..
        } = self;
        remove_build(&state, &id)?;
        drop(build_lock);
        let Some(lock) = state_lock else {
            return Ok(false);
        };
        abandon_unused(&state, state_path, &lock)
    }

    /// Empties the state directory `state`, for `ranikust prune --all`
    /// (ADR-0105): every entry, lock and build directory, and the marker
    /// last, but a directory of the user namespace probe that a probe
    /// holds, which stays. `false` when it was empty already.
    ///
    /// Takes the state directory's `flock` exclusive without waiting, as
    /// an anonymous store does, and holds it to the end: every build's
    /// store holds it shared for its life, so nothing a running build uses
    /// is removed, and no build opens a store meanwhile ([`Error::Busy`]).
    /// Before removing anything it refuses this process's own `/`, by
    /// device and inode as `rnk_rootfs::prune` does, then, under the lock,
    /// the rest of what [`Store::check_prune`] refuses, in the order given
    /// there. It takes one holding the regular [`PRUNING_MARKER`] a prune
    /// that did not finish leaves, which `open` refuses, and makes nothing
    /// in an empty directory, not even the marker. Then it renames
    /// [`STATE_MARKER`] to [`PRUNING_MARKER`], or takes over the one an
    /// earlier prune left, and syncs the directory before deleting: entries
    /// are emptied in place, and a prune cut short or failed leaves a
    /// directory no store opens and the next prune accepts. A failed sync
    /// renames back only a marker this call renamed: one an earlier prune
    /// left stays, since that prune may have removed entries. Deletes
    /// through `rnk-rootfs` by descriptor: no symlink followed, no mount
    /// crossed or emptied; the filesystem is synced before
    /// [`PRUNING_MARKER`] goes, which a failed sync keeps.
    ///
    /// The caller has checked `state` as a build's state directory: no
    /// other user may write it or a directory above it (`ranikust`'s
    /// state-directory checks, ADR-0050), since it is resolved by path
    /// here, and the checks above are all this function adds. Files of ids
    /// the process may not remove (a rootless build's, of subordinate ids)
    /// need the caller in the build's user namespace, whose root removes
    /// them; nothing here knows which namespace it runs in.
    ///
    /// # Errors
    ///
    /// [`Error::InUse`] while a build holds the state directory;
    /// [`Error::NotAStateDir`] for a name the store does not make or a
    /// [`PRUNING_MARKER`] that is not a regular file, else as
    /// [`Store::open`], as are [`Error::UsedOnSlash`] and
    /// [`Error::NoFlock`]; [`Error::Rootfs`] for this process's own `/`;
    /// [`Error::NotPruned`] naming what is left when anything but
    /// [`PRUNING_MARKER`] and a held probe directory stays (an entry on
    /// another mount, one this process may not remove); [`Error::Io`] or
    /// [`Error::Rootfs`] when the
    /// directory cannot be resolved, opened or synced, or the marker
    /// renamed.
    pub fn prune_all(state: &Path) -> Result<bool> {
        let span = tracing::info_span!("rnk_store.prune");
        let _entered = span.enter();
        let (state_path, root) = open_to_prune(state)?;
        let lock = root.open_at(".", &OpenHow::read().directory().no_follow())?;
        match lock::try_lock(&lock) {
            Ok(true) => {}
            Ok(false) => return Err(Error::InUse { path: state_path }),
            Err(source) => {
                return Err(Error::NoFlock {
                    path: state_path,
                    source,
                });
            }
        }
        empty_locked(&root, state_path, &lock)
    }

    /// [`Store::prune_all`]'s checks of the state directory `state`, run
    /// without its lock and changing nothing, with the directory's
    /// identity, for a caller that checks it before entering the user
    /// namespace `prune_all` runs in, or prints commands that act on it;
    /// `prune_all` checks again under its lock, since a build may change
    /// the directory meanwhile. `state` is opened as given, with no symlink
    /// followed anywhere on it (`RootPath::open`), not resolved first. In
    /// order, it refuses this process's own `/`; [`BUILDING_MARKER`] in any
    /// form ([`Error::UsedOnSlash`]); then, unless the directory is empty,
    /// a [`PRUNING_MARKER`] that is not a regular file, a directory with
    /// neither it nor a regular [`STATE_MARKER`], and a top-level name
    /// neither the store nor a build makes there (the markers, the store's
    /// directories, [`SANDBOX_DIR`], an anonymous store's names), each
    /// [`Error::NotAStateDir`]. Unlike `prune_all`, it needs no check of
    /// `state` by the caller: it follows no symlink on it and changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// As [`Store::prune_all`], but for [`Error::InUse`], [`Error::NoFlock`],
    /// the [`Error::Io`] of resolving `state` and what deleting returns;
    /// [`Error::Rootfs`] when `state` cannot be opened as given with no
    /// symlink followed on it ([`Error::is_symlink_on`] tells a symlink on
    /// its path), or holds a symlink.
    pub fn check_prune(state: &Path) -> Result<PruneCheck> {
        let root = RootPath::open(state)?;
        rnk_rootfs::refuse_own_root(&root, state)?;
        let found = prune_check(&root, state)?;
        let st = root.statx("")?;
        Ok(PruneCheck {
            entries: found != Prunable::Empty,
            dev: rustix::fs::makedev(st.dev_major, st.dev_minor),
            ino: st.ino,
            uid: st.uid,
        })
    }

    /// Removes the `tmp/` and `builds/` directories of every build that no
    /// longer holds its lock. Anything that fails for one build (its lock
    /// file, its directories) is left with a warning: it must not stop
    /// this build.
    fn cleanup(&self) -> Result<()> {
        let span = tracing::info_span!("rnk_store.cleanup", entries_removed = Empty);
        let _entered = span.enter();
        let mut ids = BTreeSet::new();
        for name in list(&self.state, Path::new("builds"))? {
            let lock = name.as_encoded_bytes().ends_with(b".lock");
            if !lock {
                ids.insert(name);
            }
        }
        ids.extend(list(&self.state, Path::new("tmp"))?);
        ids.remove(&OsString::from(&self.id));
        let mut removed = 0_u64;
        for id in ids {
            let Some(id) = id.to_str() else {
                tracing::warn!(target: "rnk_store", "state directory: skipping a non-UTF-8 build id {id:?}");
                continue;
            };
            match self.remove_if_dead(id) {
                Ok(n) => removed += n,
                Err(e) => {
                    tracing::warn!(target: "rnk_store", "state directory: leftovers of build {id} not removed: {e}");
                }
            }
        }
        span.record("entries_removed", removed);
        Ok(())
    }

    /// Removes the directories of build `id` if its lock is free; returns
    /// how many there were.
    fn remove_if_dead(&self, id: &str) -> Result<u64> {
        let lock_rel = lock_file(id);
        let fd = lock::open(&self.state, &lock_rel)?;
        if !lock::try_lock(&fd).map_err(|e| self.io_err("lock", &lock_rel, e))? {
            return Ok(0); // a running build
        }
        remove_build(&self.state, id)
    }

    /// Takes the per-digest lock of `key`, waiting for it: first among this
    /// store's threads, then with `flock` among processes.
    fn lock(&self, key: &EntryKey) -> Result<(lock::Held<'_>, OwnedFd)> {
        let span = tracing::info_span!("rnk_store.lock", contended = Empty);
        let _entered = span.enter();
        let rel = key.lock_path();
        let (held, waited) = self.held.acquire(&rel);
        if let Some(parent) = rel.parent() {
            self.state.create_dir_all(parent, DIR_MODE)?;
        }
        let fd = lock::open(&self.state, &rel)?;
        let free = lock::try_lock(&fd).map_err(|e| self.io_err("lock", &rel, e))?;
        if !free {
            lock::lock(&fd).map_err(|e| self.io_err("lock", &rel, e))?;
        }
        span.record("contended", waited || !free);
        Ok((held, fd))
    }

    /// The size of the published entry `key` (0 for a layer), or `None`.
    /// Only a regular file (blob) or a directory (layer) on the state
    /// directory's own mount counts; anything else under the name is an
    /// error, never followed and never replaced.
    fn published(&self, key: &EntryKey) -> Result<Option<u64>> {
        let rel = key.path();
        let st = match self.state.statx(&rel) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let (want, size) = match key {
            EntryKey::Blob(_) => (FileType::Regular, st.size),
            EntryKey::Layer(_) => (FileType::Directory, 0),
        };
        let reason = if st.file_type() != want {
            "not a regular file for a blob, or not a directory for a layer"
        } else if st.mount_root || st.mnt_id != self.state.mnt_id() {
            "on another mount than the state directory"
        } else {
            return Ok(Some(size));
        };
        Err(Error::BadEntry { path: rel, reason })
    }

    fn create_tmp(&self, key: &EntryKey, tmp: &Path) -> Result<TmpEntry> {
        let (file, root) = match key {
            EntryKey::Blob(_) => {
                let fd = self
                    .state
                    .open_at(tmp, &OpenHow::read_write().create_new(0o644))?;
                (Some(File::from(fd)), None)
            }
            EntryKey::Layer(_) => {
                self.state.create_dir(tmp, 0o755)?;
                (None, Some(self.state.sub_root(tmp)?))
            }
        };
        Ok(TmpEntry {
            path: self.host(tmp),
            file,
            root,
        })
    }

    fn publish(&self, entry: TmpEntry, tmp: &Path, rel: &Path) -> Result<()> {
        if let Some(file) = &entry.file {
            file.sync_all().map_err(|e| self.io_err("fsync", tmp, e))?;
        }
        drop(entry);
        self.state.rename(tmp, rel)?;
        Ok(())
    }

    /// Removes a temporary entry after a failure; what stays is removed by
    /// [`Store::close`] or the next start.
    fn discard(&self, tmp: &Path) {
        if let Err(e) = remove_tree(&self.state, tmp) {
            tracing::warn!(target: "rnk_store", "state directory: {} not removed: {e}", tmp.display());
        }
    }

    fn host(&self, rel: &Path) -> PathBuf {
        self.state_path.join(rel)
    }

    fn io_err(&self, op: &'static str, rel: &Path, source: io::Error) -> Error {
        Error::Io {
            op,
            path: self.host(rel),
            source,
        }
    }
}

/// `builds/<id>.lock`, held by the build `id` while it runs.
fn lock_file(id: &str) -> PathBuf {
    PathBuf::from("builds").join(format!("{id}.lock"))
}

/// The names in the directory `rel` of the state directory (`.` for the
/// state directory itself), `.` and `..` excluded. A symlink is refused.
fn list(state: &RootPath, rel: &Path) -> Result<Vec<OsString>> {
    let fd = state.open_at(rel, &OpenHow::read().directory().no_follow())?;
    let err = |e: Errno| Error::Io {
        op: "read directory",
        path: rel.to_owned(),
        source: e.into(),
    };
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&fd).map_err(err)? {
        let name = entry.map_err(err)?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name));
        }
    }
    Ok(names)
}

/// What [`read_building_marker`] found in the state directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    /// No [`BUILDING`] marker.
    Absent,
    /// This container's marker (or anything but a regular file): a build
    /// here was cut short.
    Ours,
    /// A marker from another PID namespace or boot: no process of its build
    /// is left here.
    Theirs,
}

/// The [`BUILDING`] marker in the state directory `root` (shown as
/// `state_path`), `mark` being what this build would write
/// ([`building_mark`]). A marker from another PID namespace or boot (a state
/// directory on a volume, an earlier boot) is [`Marker::Theirs`]. Read as
/// [`marker_text`] does; anything but a regular file counts as this
/// container's.
fn read_building_marker(root: &RootPath, state_path: &Path, mark: Option<&str>) -> Result<Marker> {
    Ok(match marker_text(root, state_path, 256)? {
        None => Marker::Absent,
        Some(None) => Marker::Ours,
        // An empty one is a marker this process made whose text could not
        // be written (a full filesystem).
        Some(Some(held)) if held.is_empty() || mark.is_none_or(|m| held == m.as_bytes()) => {
            Marker::Ours
        }
        Some(Some(_)) => Marker::Theirs,
    })
}

/// What [`BUILDING`] in the state directory `root` holds, its first `max`
/// bytes: `None` when nothing has the name, `Some(None)` when what has it
/// is not a regular file. The entry is pinned with `O_PATH | O_NOFOLLOW`
/// and its type checked on that descriptor before anything opens it for
/// reading (`rnk_rootfs::Pinned::open_regular`): a link is never followed,
/// and a FIFO or device node a `RUN` put there is never opened.
fn marker_text(root: &RootPath, state_path: &Path, max: u64) -> Result<Option<Option<Vec<u8>>>> {
    let pinned = match root.pin(BUILDING) {
        Ok(p) => p,
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if pinned.stat().file_type() != FileType::Regular {
        return Ok(Some(None));
    }
    let mut held = Vec::new();
    io::Read::take(File::from(pinned.open_regular()?), max)
        .read_to_end(&mut held)
        .map_err(|e| building_marker_err(state_path, "read", e))?;
    Ok(Some(Some(held)))
}

/// Creates this build's [`BUILDING`] marker, holding `mark`, as a new file.
fn write_building_marker(root: &RootPath, state_path: &Path, mark: Option<&str>) -> Result<()> {
    let mut f = File::from(root.open_at(BUILDING, &OpenHow::write().create_new(0o600))?);
    io::Write::write_all(&mut f, mark.unwrap_or_default().as_bytes())
        .map_err(|e| building_marker_err(state_path, "write", e))
}

/// Creates [`BUILDING`] holding `mark` through the directory descriptor
/// `dir`, never following a link; `false` when something already holds the
/// name.
fn create_building_marker(dir: &OwnedFd, state_path: &Path, mark: Option<&str>) -> Result<bool> {
    #[cfg(test)]
    if tests::marker_create_fails(dir) {
        return Err(building_marker_err(
            state_path,
            "create",
            Errno::NOSPC.into(),
        ));
    }
    let fd = match rustix::fs::openat(
        dir,
        BUILDING,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    ) {
        Ok(fd) => fd,
        Err(Errno::EXIST) => return Ok(false),
        Err(e) => return Err(building_marker_err(state_path, "create", e.into())),
    };
    io::Write::write_all(&mut File::from(fd), mark.unwrap_or_default().as_bytes())
        .map_err(|e| building_marker_err(state_path, "write", e))?;
    Ok(true)
}

/// Whether [`BUILDING`] is a regular file holding exactly `mark`, read as
/// [`marker_text`] does: no link followed, no FIFO or device opened.
fn marker_is_mine(root: &RootPath, state_path: &Path, mark: Option<&str>) -> Result<Option<bool>> {
    Ok(marker_text(root, state_path, 257)?
        .map(|held| held.is_some_and(|h| h == mark.unwrap_or_default().as_bytes())))
}

/// Makes [`BUILDING`] this build's marker: a regular file holding `mark`.
/// Anything else under the name (nothing, a link, a FIFO, a directory a
/// `RUN` filled, another build's text) is removed through `rnk-rootfs`,
/// never followed, and a new file made. Returns whether it had to: the
/// marker was not the one this build wrote (ADR-0077).
fn ensure_own_marker(
    root: &RootPath,
    dir: &OwnedFd,
    state_path: &Path,
    mark: Option<&str>,
) -> Result<bool> {
    // A `RUN` process left behind could race the replacement: a few tries,
    // then the error.
    for _ in 0..3 {
        match marker_is_mine(root, state_path, mark)? {
            Some(true) => return Ok(false),
            Some(false) => {
                remove_tree(root, Path::new(BUILDING))?;
            }
            None => {}
        }
        if create_building_marker(dir, state_path, mark)? {
            return Ok(true);
        }
    }
    Err(building_marker_err(
        state_path,
        "replace",
        io::Error::other("something keeps taking its name"),
    ))
}

/// What an anonymous store found in a state directory besides its own
/// names, for [`Error::StateNotEmpty`].
fn describe_foreign(names: &[OsString]) -> String {
    let has = |prefix: &str| {
        names
            .iter()
            .any(|n| n.as_encoded_bytes().starts_with(prefix.as_bytes()))
    };
    if names.iter().any(|n| n == MARKER) {
        format!(
            "a persistent store ({MARKER}, among {})",
            shown_names(names)
        )
    } else if has(".nfs") || has(".fuse_hidden") {
        format!(
            "{}: names NFS or FUSE give a file removed while open; such a state directory is \
             not supported, and ranikust left them there itself only if an earlier build ran on \
             it",
            shown_names(names)
        )
    } else {
        format!("{}, which ranikust did not leave there", shown_names(names))
    }
}

/// Removes everything in an anonymous store's state directory but
/// [`BUILDING`]: nothing there is the store's own at its end, so whatever
/// is left a `RUN` wrote (ADR-0077). An entry on another mount stays, and
/// fails the call, as any entry that could not be removed does; a
/// directory of the user namespace probe that a probe holds stays, and
/// fails nothing.
fn empty_state_dir(state: &RootPath, state_path: &Path) -> Result<()> {
    remove_state_marker(state);
    let found: Vec<OsString> = list(state, Path::new("."))?
        .into_iter()
        .filter(|n| n != BUILDING)
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    let mut keep = IgnoreList::new();
    keep.insert(BUILDING)?;
    let (deleted, held) = delete_contents_but_probes(state, &keep);
    let found: Vec<OsString> = found.into_iter().filter(|n| !held.contains(n)).collect();
    if !found.is_empty() {
        tracing::warn!(
            target: "rnk_store",
            "state directory {}: {} entries a RUN left there ({}){}; kaniko mode on / keeps \
             nothing there between builds (ADR-0077)",
            state_path.display(),
            found.len(),
            shown_names(&found),
            if deleted.is_ok() { " removed" } else { "" },
        );
    }
    let left: Vec<OsString> = list(state, Path::new("."))?
        .into_iter()
        .filter(|n| n != BUILDING && !held.contains(n))
        .collect();
    match (left.is_empty(), deleted) {
        (true, Ok(())) => Ok(()),
        (_, Err(e)) => Err(Error::NotEmptied {
            path: state_path.to_owned(),
            left: format!("{} (removing them failed: {e})", shown_names(&left)),
        }),
        (false, Ok(())) => Err(Error::NotEmptied {
            path: state_path.to_owned(),
            left: format!("{} (on another mount?)", shown_names(&left)),
        }),
    }
}

/// Removes a regular [`MARKER`] a `RUN` forged in an anonymous store's state
/// directory, before anything else there: without it, `needs_marker` and
/// `ranikust`'s state-directory check refuse a non-empty directory, so a
/// persistent store never opens what is left if the build is killed while
/// the rest is emptied. Anything else under the name, or a failure, is left
/// to the general emptying.
fn remove_state_marker(state: &RootPath) {
    if state
        .statx(MARKER)
        .is_ok_and(|st| st.file_type() == FileType::Regular)
    {
        let _ = state.remove_file(MARKER);
    }
}

/// An [`Error::Io`] on the [`BUILDING`] marker.
fn building_marker_err(state_path: &Path, op: &'static str, source: io::Error) -> Error {
    Error::Io {
        op,
        path: state_path.join(BUILDING),
        source,
    }
}

/// Checks that `/proc/self/fd`, through which the names an anonymous store
/// hands out resolve, is procfs, not something mounted over `/proc` (a
/// `RUN` could only do that with `CAP_SYS_ADMIN`, which kaniko mode on `/`
/// never leaves it).
fn check_proc_fd() -> Result<()> {
    let proc_fd = Path::new("/proc/self/fd");
    let proc_err = |e: io::Error| Error::Io {
        op: "check",
        path: proc_fd.to_owned(),
        source: e,
    };
    let fs = rustix::fs::statfs(proc_fd).map_err(|e| proc_err(e.into()))?;
    if fs.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(proc_err(io::Error::other("not procfs")));
    }
    Ok(())
}

/// Records on an `rnk_store.fill` span that the entry was there already,
/// `size` bytes.
fn record_reused(span: &tracing::Span, size: u64) {
    span.record("reused", true);
    span.record("bytes_written", 0);
    span.record("bytes_reused", size);
}

/// Records on an `rnk_store.fill` span that the filler wrote `bytes`.
fn record_written(span: &tracing::Span, bytes: u64) {
    span.record("reused", false);
    span.record("bytes_written", bytes);
    span.record("bytes_reused", 0);
}

/// Takes the exclusive `flock` on the state directory `state_path` through
/// `fd`, its descriptor, without waiting.
fn lock_state_dir(fd: &OwnedFd, state_path: &Path) -> Result<()> {
    match lock::try_lock(fd) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Busy {
            path: state_path.to_owned(),
        }),
        Err(source) => Err(Error::NoFlock {
            path: state_path.to_owned(),
            source,
        }),
    }
}

/// Refuses, for a persistent store, a state directory holding
/// [`BUILDING`] in any form: a kaniko-mode build on `/` used it and did not
/// end cleanly, or still runs there under another lock (ADR-0077).
fn refuse_used_on_slash(state: &RootPath, state_path: &Path) -> Result<()> {
    match state.statx(BUILDING) {
        Ok(_) => Err(Error::UsedOnSlash {
            path: state_path.to_owned(),
        }),
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Refuses a state directory holding [`PRUNING`], whatever its type: a
/// `prune --all` that did not finish may have emptied entries part-way.
fn refuse_cut_short_prune(state: &RootPath, state_path: &Path) -> Result<()> {
    match state.statx(PRUNING) {
        Ok(_) => Err(Error::PruneCutShort {
            path: state_path.to_owned(),
        }),
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Empties the state directory `state`, whose shared `flock` `lock`
/// holds, where [`Store::abandon`] would: `false` where another build
/// holds it too, or [`empty_if_unused`] keeps it.
fn abandon_unused(state: &RootPath, state_path: PathBuf, lock: &OwnedFd) -> Result<bool> {
    // A conversion: the shared lock goes, and the exclusive one comes only
    // where no other descriptor holds the directory.
    match lock::try_lock(lock) {
        Ok(true) => {}
        Ok(false) => return Ok(false),
        Err(source) => {
            return Err(Error::NoFlock {
                path: state_path,
                source,
            });
        }
    }
    empty_if_unused(state, &state_path, lock)
}

/// Empties the state directory `state`, whose exclusive `flock` `lock`
/// holds, where it is a store with no data: [`MARKER`] and nothing in
/// [`DATA_DIRS`], neither [`PRUNING`] nor [`BUILDING`], and only names
/// the store makes. [`MARKER`] goes last, after the sync. `false` where it
/// is anything else, which is kept as found, and where a probe still holds
/// a directory of the user namespace probe there: kept, those that no
/// probe holds removed.
fn empty_if_unused(state: &RootPath, state_path: &Path, lock: &OwnedFd) -> Result<bool> {
    // Under the lock: no build adds data or names meanwhile.
    match prune_check(state, state_path) {
        Ok(Prunable::Marked) => {}
        // Not a store, or one a kaniko-mode build on `/` or a prune used:
        // kept as found, for the caller's own refusal or none.
        Ok(Prunable::Empty | Prunable::CutShort)
        | Err(Error::NotAStateDir { .. } | Error::UsedOnSlash { .. }) => return Ok(false),
        Err(e) => return Err(e),
    }
    if holds_data(state)? {
        return Ok(false);
    }
    let span = tracing::info_span!("rnk_store.prune");
    let _entered = span.enter();
    rnk_rootfs::refuse_own_root(state, state_path)?;
    let mut held = None;
    for name in list(state, Path::new("."))? {
        match remove_probe_dir(state, Path::new(&name))? {
            ProbeDir::Held => held = Some(name),
            ProbeDir::Removed | ProbeDir::NotOne => {}
        }
    }
    if let Some(name) = held {
        tracing::info!(
            target: "rnk_store",
            "state directory {}: the store holds no data but is kept: a user namespace probe \
             still uses {name:?} there",
            state_path.display()
        );
        return Ok(false);
    }
    empty_all_but(state, state_path.to_owned(), lock, MARKER, |path, left| {
        Error::NotAbandoned { path, left }
    })?;
    Ok(true)
}

/// The store's directories whose entries builds reuse: blobs, extracted
/// layers, cache mounts; the rest of [`DIRS`] holds only what a build
/// uses while it runs.
const DATA_DIRS: [&str; 3] = ["blobs", "layers", "caches"];

/// Whether the state directory `root` holds an entry in one of
/// [`DATA_DIRS`]; a missing one holds none, and one that is not a
/// directory is an error.
fn holds_data(root: &RootPath) -> Result<bool> {
    for dir in DATA_DIRS {
        match root.statx(dir) {
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => continue,
            st => drop(st?),
        }
        if !list(root, Path::new(dir))?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Logs what a failed [`Store::open_or_abandon`] did with the store in
/// `state_path`.
fn log_abandoned(state_path: &Path, abandoned: Result<bool>) {
    match abandoned {
        Ok(true) => tracing::debug!(
            target: "rnk_store",
            "state directory {}: the store, which holds no data, is removed",
            state_path.display()
        ),
        Ok(false) => tracing::debug!(
            target: "rnk_store",
            "state directory {}: the store is kept: it holds data or a name no store makes, or \
             another build holds it",
            state_path.display()
        ),
        Err(e) => tracing::warn!(
            target: "rnk_store",
            "state directory {}: the store is not removed: {e}",
            state_path.display()
        ),
    }
}

/// [`Store::prune_all`] once its exclusive `lock` on the state directory
/// `root` is taken: checks it again, then empties it, the marker last.
fn empty_locked(root: &RootPath, state_path: PathBuf, lock: &OwnedFd) -> Result<bool> {
    let renamed = match prune_check(root, &state_path)? {
        Prunable::Empty => return Ok(false),
        // A prune cut short: this one finishes it.
        Prunable::CutShort => false,
        Prunable::Marked => {
            // No new inode, which a full disk or quota would refuse;
            // not RENAME_NOREPLACE, which NFS refuses (the flock is ours).
            root.rename(MARKER, PRUNING)?;
            true
        }
    };
    // On disk before any entry is emptied in place, even when an
    // earlier prune renamed it and was killed before its sync.
    if let Err(e) = rustix::fs::fsync(lock) {
        // Only this call's: an earlier prune's may cover removed entries.
        if renamed && let Err(back) = root.rename(PRUNING, MARKER) {
            tracing::warn!(
                target: "rnk_store",
                "state directory {}: {PRUNING} cannot be renamed back to {MARKER} \
                 ({back}); builds refuse the directory until ranikust prune --all empties it",
                state_path.display()
            );
        }
        return Err(Error::Io {
            op: "sync",
            path: state_path,
            source: e.into(),
        });
    }
    empty_all_but(root, state_path, lock, PRUNING, |path, left| {
        Error::NotPruned { path, left }
    })?;
    Ok(true)
}

/// Deletes everything in the state directory `root` but `last`, whose
/// exclusive `lock` is held, then, once the filesystem is synced, `last`.
/// What stays besides `last` is the error `left_err` makes, `last` kept,
/// but for a directory of the user namespace probe that a probe holds,
/// which stays and fails nothing.
fn empty_all_but(
    root: &RootPath,
    state_path: PathBuf,
    lock: &OwnedFd,
    last: &str,
    left_err: fn(PathBuf, String) -> Error,
) -> Result<()> {
    let mut keep = IgnoreList::new();
    keep.insert(last)?;
    let (deleted, held) = delete_contents_but_probes(root, &keep);
    let left: Vec<OsString> = list(root, Path::new("."))?
        .into_iter()
        .filter(|n| n != last && !held.contains(n))
        .collect();
    let left = match (left.is_empty(), deleted) {
        (true, Ok(())) => None,
        (true, Err(e)) => Some(format!("nothing it can name (removing failed: {e})")),
        (false, Err(e)) => Some(format!(
            "{} (removing them failed: {e})",
            shown_names(&left)
        )),
        (false, Ok(())) => Some(format!("{} (on another mount?)", shown_names(&left))),
    };
    if let Some(left) = left {
        return Err(left_err(state_path, left));
    }
    // The removals on disk before the marker's, which a filesystem may
    // otherwise keep alone: an unmarked, non-empty directory.
    rustix::fs::syncfs(lock).map_err(|e| Error::Io {
        op: "sync",
        path: state_path,
        source: e.into(),
    })?;
    root.remove_file(last)?;
    Ok(())
}

/// The state directory `state` resolved and opened for
/// [`Store::prune_all`], this process's own `/` refused.
fn open_to_prune(state: &Path) -> Result<(PathBuf, RootPath)> {
    let state_path = fs::canonicalize(state).map_err(|source| Error::Io {
        op: "resolve",
        path: state.to_owned(),
        source,
    })?;
    let root = RootPath::open(&state_path)?;
    rnk_rootfs::refuse_own_root(&root, &state_path)?;
    Ok((state_path, root))
}

/// A state directory [`Store::check_prune`] accepts: whether it holds
/// anything, and which directory it is, as stat(2) and `stat -c
/// %d:%i:%u` give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneCheck {
    /// It holds something for [`Store::prune_all`] to empty.
    pub entries: bool,
    /// Its `st_dev`.
    pub dev: u64,
    /// Its inode number.
    pub ino: u64,
    /// Its owner, as this process's user namespace maps it.
    pub uid: u32,
}

/// What [`prune_check`] found in a state directory it accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prunable {
    /// Nothing.
    Empty,
    /// The store's regular [`MARKER`].
    Marked,
    /// The regular [`PRUNING`] a prune that did not finish leaves.
    CutShort,
}

/// [`Store::prune_all`]'s checks of the state directory `root`, which
/// change nothing, in one place for it and [`Store::check_prune`], whose
/// documentation lists them.
fn prune_check(root: &RootPath, state_path: &Path) -> Result<Prunable> {
    refuse_used_on_slash(root, state_path)?;
    let names = list(root, Path::new("."))?;
    if names.is_empty() {
        return Ok(Prunable::Empty);
    }
    let refuse = |why: String| Error::NotAStateDir {
        path: state_path.to_owned(),
        why,
    };
    let regular = |name: &str| -> Result<Option<bool>> {
        match root.statx(name) {
            Ok(st) => Ok(Some(st.file_type() == FileType::Regular)),
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    // The markers first: a directory without one is no state directory,
    // whatever else it holds.
    let found = match (regular(PRUNING)?, regular(MARKER)?) {
        (Some(true), _) => Prunable::CutShort,
        (Some(false), _) => {
            return Err(refuse(format!(
                "holds {PRUNING}, which is not a regular file and which ranikust prune --all \
                 neither takes nor removes: remove it first"
            )));
        }
        (None, Some(true)) => Prunable::Marked,
        (None, Some(false) | None) => {
            return Err(refuse(
                "is not empty and has no regular file .ranikust-state, so ranikust prune --all \
                 removes nothing: it empties only a state directory ranikust made"
                    .to_owned(),
            ));
        }
    };
    match foreign_names(names.iter().map(OsString::as_os_str)) {
        Some(why) => Err(refuse(why)),
        None => Ok(found),
    }
}

/// Why [`Store::prune_all`] refuses a state directory whose top-level
/// `names` include one neither the store nor a build makes there (the
/// markers, the store's directories, [`SANDBOX_DIR`], an anonymous store's
/// names): the `why` of [`Error::NotAStateDir`], naming the first few.
/// `None` when the store makes them all.
fn foreign_names<'a>(names: impl IntoIterator<Item = &'a OsStr>) -> Option<String> {
    let foreign: Vec<OsString> = names
        .into_iter()
        .filter(|n| !is_store_name(n))
        .map(OsStr::to_owned)
        .collect();
    (!foreign.is_empty()).then(|| {
        format!(
            "holds {}, which ranikust does not make there, so ranikust prune --all removes \
             nothing: move what ranikust did not make out of it first",
            shown_names(&foreign)
        )
    })
}

/// Whether `name`, at the top of a state directory, is one the store or a
/// build makes there.
fn is_store_name(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    [MARKER, PRUNING, BUILDING, SANDBOX_DIR]
        .iter()
        .chain(&DIRS)
        .any(|own| name == own.as_bytes())
        || name.starts_with(anon::PREFIX.as_bytes())
}

/// The first few of `names`, quoted, for a log line: names a `RUN` chose.
fn shown_names(names: &[OsString]) -> String {
    const SHOWN: usize = 5;
    let mut s = names
        .iter()
        .take(SHOWN)
        .map(|n| format!("{n:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        s.push_str(", …");
    }
    s
}

/// Writes the marker in a directory [`needs_marker`] found unmarked; where
/// a build starting at the same time wrote it first, takes the directory
/// if that marker is a regular file.
fn mark(state: &RootPath, state_path: &Path) -> Result<()> {
    match state.open_at(MARKER, &OpenHow::write().create_new(0o644)) {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => match marker(state)? {
            Some(true) => Ok(()),
            _ => Err(not_a_state_dir(state_path)),
        },
        Err(e) => Err(e.into()),
    }
}

/// Whether `state` still needs its marker, writing nothing: `false` where
/// it has a regular-file one, `true` where it holds only directories of
/// the user namespace probe ([`is_probe_dir`]: of a build cut short before
/// its store); any other name refuses it.
fn needs_marker(state: &RootPath, state_path: &Path) -> Result<bool> {
    match marker(state)? {
        Some(true) => return Ok(false),
        Some(false) => return Err(not_a_state_dir(state_path)),
        None => {}
    }
    let mut only_probes = true;
    for name in list(state, Path::new("."))? {
        if !is_probe_dir(state, &name)? {
            only_probes = false;
            break;
        }
    }
    if only_probes {
        return Ok(true);
    }
    // A build creating the store concurrently writes the marker first.
    match marker(state)? {
        Some(true) => Ok(false),
        _ => Err(not_a_state_dir(state_path)),
    }
}

/// Whether `name`, at the top of `state`, is a directory of the user
/// namespace probe: named so ([`USERNS_PROBE_PREFIX`]), and
/// [`is_probe_stat`]. One with that name gone since it was listed counts
/// as one: an absent entry cannot make the directory foreign.
fn is_probe_dir(state: &RootPath, name: &OsStr) -> Result<bool> {
    if !is_probe_name(name) {
        return Ok(false);
    }
    match state.statx(name) {
        Ok(st) => Ok(is_probe_stat(state, &st)),
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// Whether `name` is one the user namespace probe gives its directory.
pub(crate) fn is_probe_name(name: &OsStr) -> bool {
    name.as_encoded_bytes()
        .starts_with(USERNS_PROBE_PREFIX.as_bytes())
}

/// Whether `st`, taken without following a symlink, is that of a
/// directory on `state`'s mount that is not a mount point.
fn is_probe_stat(state: &RootPath, st: &Stat) -> bool {
    st.file_type() == FileType::Directory && !st.mount_root && st.mnt_id == state.mnt_id()
}

/// Removes each directory of the user namespace probe ([`is_probe_dir`])
/// that no probe holds: opened without following a symlink, it takes the
/// directory's exclusive `flock`, and is the directory listed, then
/// removed with its contents through descriptors. A failure is a warning:
/// it must not stop this build.
fn remove_probe_leftovers(state: &RootPath, state_path: &Path) {
    let names = match list(state, Path::new(".")) {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(target: "rnk_store", "state directory {}: cannot be listed for the user namespace probe's leftovers: {e}", state_path.display());
            return;
        }
    };
    for name in names {
        let rel = Path::new(&name);
        match remove_probe_dir(state, rel) {
            Ok(ProbeDir::Removed) => tracing::info!(
                target: "rnk_store",
                "removed {} left by an earlier build",
                state_path.join(rel).display()
            ),
            Ok(ProbeDir::Held | ProbeDir::NotOne) => {}
            Err(e) => tracing::warn!(
                target: "rnk_store",
                "{}",
                probe_dir_not_removed(&state_path.join(rel), &e)
            ),
        }
    }
}

/// What [`remove_probe_leftovers`] logs of the directory of the user
/// namespace probe at `path` it could not remove for `e`.
fn probe_dir_not_removed(path: &Path, e: &Error) -> String {
    format!(
        "{}, a directory of the user namespace probe, not removed (an earlier build's, or one \
         a starting build still uses): {e}",
        path.display()
    )
}

/// What [`remove_probe_dir`] and [`remove_probe_named`] did with a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeDir {
    /// Removed, a directory of the probe under its own lock.
    Removed,
    /// A directory of the probe that a probe holds, or that one became
    /// while it was looked at: kept.
    Held,
    /// No directory of the probe, or gone: left as found.
    NotOne,
}

/// [`remove_probe_leftovers`] for `rel`: removed only once its exclusive
/// `flock` is taken.
pub(crate) fn remove_probe_dir(state: &RootPath, rel: &Path) -> Result<ProbeDir> {
    if !is_probe_name(rel.as_os_str()) {
        return Ok(ProbeDir::NotOne);
    }
    let st = match state.statx(rel) {
        Ok(st) if is_probe_stat(state, &st) => st,
        Ok(_) => return Ok(ProbeDir::NotOne),
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
            return Ok(ProbeDir::NotOne);
        }
        Err(e) => return Err(e.into()),
    };
    #[cfg(test)]
    tests::probe_dir_listed(state, rel);
    let held = match state.open_at(rel, &OpenHow::read().directory().no_follow()) {
        Ok(held) => held,
        // Removed since it was listed: by its probe, or by another store.
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
            return Ok(ProbeDir::NotOne);
        }
        Err(e) => return Err(e.into()),
    };
    let io = |source| Error::Io {
        op: "lock",
        path: rel.to_owned(),
        source,
    };
    if !lock::try_lock(&held).map_err(io)? {
        // A probe uses it.
        return Ok(ProbeDir::Held);
    }
    let locked = rustix::fs::fstat(&held).map_err(|e| io(e.into()))?;
    let listed = rustix::fs::makedev(st.dev_major, st.dev_minor);
    if (locked.st_dev, locked.st_ino) != (listed, st.ino) {
        return Err(Error::BadEntry {
            path: rel.to_owned(),
            reason: "changed while it was being locked; not deleted",
        });
    }
    remove_dir_tree(state, rel, &st)?;
    Ok(ProbeDir::Removed)
}

/// Removes `rel`, a name of the user namespace probe ([`is_probe_name`])
/// at the top of `state`, as an emptying of the directory does: one of the
/// probe's directories only through [`remove_probe_dir`], anything else of
/// that name (a file a `RUN` made) as any entry, never followed.
pub(crate) fn remove_probe_named(state: &RootPath, rel: &Path) -> Result<ProbeDir> {
    if !is_probe_name(rel.as_os_str()) {
        return Ok(ProbeDir::NotOne);
    }
    match remove_probe_dir(state, rel)? {
        ProbeDir::NotOne => {}
        done @ (ProbeDir::Removed | ProbeDir::Held) => return Ok(done),
    }
    let st = match state.statx(rel) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
            return Ok(ProbeDir::NotOne);
        }
        Err(e) => return Err(e.into()),
    };
    if is_probe_stat(state, &st) {
        // One of the probe's again since it was looked at.
        return Ok(ProbeDir::Held);
    }
    if st.file_type() == FileType::Directory {
        // A mount point: `remove_dir_tree` refuses it, naming it.
        remove_dir_tree(state, rel, &st)?;
    } else {
        state.remove_file(rel)?;
    }
    Ok(ProbeDir::Removed)
}

/// [`delete_contents`] of the state directory `state` but what `keep`
/// covers and the user namespace probe's names, which go after it, each
/// through [`remove_probe_named`]: a directory of the probe never goes
/// while a probe holds it, even one made during the emptying. Returns the
/// first error, and the names a probe holds, which stay: the probe removes
/// them.
fn delete_contents_but_probes(state: &RootPath, keep: &IgnoreList) -> (Result<()>, Vec<OsString>) {
    let mut held = Vec::new();
    let mut keep = keep.clone();
    if let Err(e) = keep.insert_glob(format!("{USERNS_PROBE_PREFIX}*")) {
        return (Err(e.into()), held);
    }
    let mut first = delete_contents(state, &keep).map_err(Error::from);
    let names = match list(state, Path::new(".")) {
        Ok(names) => names,
        Err(e) => return (first.and(Err(e)), held),
    };
    for name in names.into_iter().filter(|n| is_probe_name(n)) {
        match remove_probe_named(state, Path::new(&name)) {
            Ok(ProbeDir::Held) => held.push(name),
            Ok(ProbeDir::Removed | ProbeDir::NotOne) => {}
            Err(e) => first = first.and(Err(e)),
        }
    }
    (first, held)
}

/// The state directory's marker: `Some(true)` for a regular file,
/// `Some(false)` for anything else of that name, `None` where there is
/// none.
fn marker(state: &RootPath) -> Result<Option<bool>> {
    match state.statx(MARKER) {
        Ok(st) => Ok(Some(st.file_type() == FileType::Regular)),
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn not_a_state_dir(state_path: &Path) -> Error {
    Error::NotAStateDir {
        path: state_path.to_owned(),
        why: UNMARKED.to_owned(),
    }
}

/// Refuses a state directory where one of [`DIRS`] is there but is not a
/// directory, each opened `O_PATH | O_NOFOLLOW`: every path the store hands
/// back would resolve through a symlink there, even one that stays inside.
/// A missing one is made later.
fn check_dirs(state: &RootPath, state_path: &Path) -> Result<()> {
    for d in DIRS {
        let fd = match state.open_at(d, &OpenHow::path().no_follow()) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => continue,
            Err(e) => return Err(e.into()),
        };
        let st = rustix::fs::fstat(&fd).map_err(|e| Error::Io {
            op: "stat",
            path: state_path.join(d),
            source: e.into(),
        })?;
        let (what, or_move) = match rustix::fs::FileType::from_raw_mode(st.st_mode) {
            rustix::fs::FileType::Directory => continue,
            rustix::fs::FileType::Symlink => (
                "a symlink",
                ", or move the directory it stands for to its name",
            ),
            rustix::fs::FileType::RegularFile => ("a regular file", ""),
            _ => ("a special file", ""),
        };
        return Err(Error::NotAStateDir {
            path: state_path.to_owned(),
            why: format!(
                "holds {d}, which is {what}, where the store keeps a directory of its own; \
                 refusing to use it as the state directory (remove {d}, and ranikust makes it \
                 again{or_move})"
            ),
        });
    }
    Ok(())
}

/// Gives `d`, one of [`DIRS`] and this user's, the effective gid and no
/// setgid bit, where a setgid state directory gave it its own gid and the
/// bit, or a `grpid` mount its parent's gid: entries below then get the gid of
/// the process that makes them, which a user namespace maps (ADR-0099),
/// and never a gid and setgid bit from the host in an extracted layer.
/// Read and changed through one pinned descriptor, and only when it needs
/// it. Outside a user namespace no map needs the gid, so a filesystem that
/// refuses the `chown` (`EPERM`) does not stop the store: a warning; in
/// one, an error.
fn own_group(state: &RootPath, d: &str) -> Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    let egid = rustix::process::getegid().as_raw();
    let changed = state.set_meta_if(d, |st| {
        let needed = st.file_type() == FileType::Directory
            && st.uid == euid
            && (st.mode & SETGID != 0 || st.gid != egid);
        needed.then(|| rnk_rootfs::Meta {
            gid: Some(egid),
            mode: Some(st.permissions() & !SETGID),
            ..rnk_rootfs::Meta::default()
        })
    });
    match changed {
        Ok(_) => Ok(()),
        Err(e) => refused_group(e, d, crate::userns::in_user_namespace),
    }
}

/// What [`own_group`] does when the change of `d` fails with `e`: a
/// warning for a refused group change (`EPERM` from the `chown` itself)
/// outside a user namespace; an error for any other failure, in one, or
/// where `in_user_namespace` cannot tell.
fn refused_group(
    e: rnk_rootfs::Error,
    d: &str,
    in_user_namespace: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    // A refused chmod is not a filesystem without groups: the chown before
    // it needs the same ownership and passed.
    let refused = matches!(&e, rnk_rootfs::Error::Io { op: "chown", .. })
        && e.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error());
    if !refused {
        return Err(e.into());
    }
    match in_user_namespace() {
        Ok(false) => {
            tracing::warn!(
                target: "rnk_store",
                "state directory: {d} keeps its group and mode: {e}"
            );
            Ok(())
        }
        Ok(true) => Err(e.into()),
        Err(ns) => {
            tracing::warn!(
                target: "rnk_store",
                "state directory: {d}: the user namespace cannot be told: {ns}"
            );
            Err(e.into())
        }
    }
}

/// Warns when the state directory or one of the store's directories is
/// owned by another user or writable by group or others: another user
/// could then plant entries that builds trust. Not an error, because a
/// shared or pre-created state directory is a deliberate set-up.
fn warn_if_shared(state: &RootPath, state_path: &Path) {
    let euid = rustix::process::geteuid().as_raw();
    for rel in std::iter::once(".").chain(DIRS) {
        let Ok(st) = state.statx(rel) else { continue };
        if st.uid != euid || st.mode & 0o022 != 0 {
            tracing::warn!(
                target: "rnk_store",
                "state directory: {} is owned by uid {} with mode {:o}; \
                 other users may be able to change what builds reuse",
                state_path.join(rel).display(),
                st.uid,
                st.permissions(),
            );
        }
    }
}

/// Picks a new build id, takes its lock and creates its directories.
fn register(state: &RootPath, state_path: &Path) -> Result<(String, OwnedFd)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    for attempt in 0_u32.. {
        let id = format!("{pid:x}-{nanos:x}-{attempt:x}");
        let rel = lock_file(&id);
        let fd = match state.open_at(&rel, &OpenHow::read().create_new(0o600)) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => continue,
            Err(e) => return Err(e.into()),
        };
        match lock::try_lock(&fd) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(source) => {
                let _ = state.remove_file(&rel);
                return Err(Error::NoFlock {
                    path: state_path.to_owned(),
                    source,
                });
            }
        }
        let created = state
            .create_dir(Path::new("builds").join(&id), DIR_MODE)
            .and_then(|()| state.create_dir(Path::new("tmp").join(&id), DIR_MODE));
        if let Err(e) = created {
            // Leave nothing behind but what the next start removes anyway.
            if let Err(e) = remove_build(state, &id) {
                tracing::warn!(target: "rnk_store", "state directory: build {id} not unregistered: {e}");
            }
            return Err(e.into());
        }
        return Ok((id, fd));
    }
    unreachable!("u32 attempts exhausted")
}

/// Removes `tmp/<id>`, `builds/<id>` and then the lock file of the build
/// `id`, whose lock the caller holds; returns how many of the two
/// directories existed.
fn remove_build(state: &RootPath, id: &str) -> Result<u64> {
    let mut removed = 0;
    for dir in ["tmp", "builds"] {
        if remove_tree(state, &Path::new(dir).join(id))? {
            removed += 1;
        }
    }
    match state.remove_file(lock_file(id)) {
        Err(e) if e.raw_os_error() != Some(Errno::NOENT.raw_os_error()) => return Err(e.into()),
        _ => {}
    }
    Ok(removed)
}

/// Removes the entry `rel` of the state directory and everything below it,
/// through `rnk-rootfs`: no symlink followed (a symlink is unlinked), no
/// mount crossed or emptied. `false` when it did not exist.
///
/// Not for `caches/`, which `RUN` writes in a persistent store, or anything
/// else a build's content can reach there, until `rnk-rootfs` offers a
/// deletion that opens the top directory itself without following or
/// crossing.
///
/// An anonymous store uses it on names a `RUN` can reach (its marker, the
/// leftovers [`anon::remove_leftovers`] removes): safe there because the
/// top entry is `statx`ed without following, mount roots and other mounts
/// are refused, and [`remove_dir_tree`] checks that the directory it opens
/// is the one seen (inode, device, mount) and empties it by descriptor;
/// and because kaniko mode on `/` gives ranikust no more authority than its
/// `RUN` before the store opens (`rnk_sandbox::trim_to_run_capabilities`,
/// ADR-0078), so whatever a swap could make this delete, the `RUN` could
/// delete itself. That last condition is load-bearing: an anonymous store
/// opened without the trim needs this reviewed again.
fn remove_tree(state: &RootPath, rel: &Path) -> Result<bool> {
    let st = match state.statx(rel) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if st.file_type() == FileType::Directory {
        remove_dir_tree(state, rel, &st)?;
    } else {
        state.remove_file(rel)?;
    }
    Ok(true)
}

/// Removes the directory `rel`, whose `statx` (taken without following)
/// is `st`, with its contents.
///
/// Refuses a mount point. The directory is then opened, and the opened
/// directory must be the one `st` describes (inode, device, mount): a
/// swap of `rel` for a symlink (to `/`, the state directory itself) or for
/// a mount between the two steps is refused instead of emptying what it
/// points to.
pub(crate) fn remove_dir_tree(state: &RootPath, rel: &Path, st: &Stat) -> Result<()> {
    let refuse = |reason| Error::BadEntry {
        path: rel.to_owned(),
        reason,
    };
    if st.mount_root || st.mnt_id != state.mnt_id() {
        return Err(refuse("a mount point; not deleted"));
    }
    let sub = state.sub_root(rel)?;
    let now = sub.statx("/")?;
    let same = now.ino == st.ino
        && (now.dev_major, now.dev_minor) == (st.dev_major, st.dev_minor)
        && now.mnt_id == st.mnt_id
        && sub.mnt_id() == state.mnt_id()
        && !now.mount_root;
    if !same {
        return Err(refuse("changed while it was being deleted; not deleted"));
    }
    delete_contents(&sub, &IgnoreList::new())?;
    drop(sub);
    state.remove_dir(rel)?;
    Ok(())
}

#[cfg(test)]
mod tests;
