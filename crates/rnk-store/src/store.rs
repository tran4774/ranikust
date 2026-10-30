//! The store: opening and clean-up, per-digest locks, publication.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use rnk_oci::Digest;
use rnk_rootfs::{FileType, IgnoreList, OpenHow, RootPath, Stat, delete_contents};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};
use tracing::field::Empty;

use crate::error::{Error, Result};
use crate::key::EntryKey;
use crate::lock;

/// The marker file of a state directory.
const MARKER: &str = ".ranikust-root";
/// The directories [`Store::open`] creates in the state directory.
const DIRS: [&str; 6] = ["blobs", "layers", "caches", "locks", "tmp", "builds"];
/// Mode of the state directory and the store's own directories: image
/// content stays away from other users of the host.
const DIR_MODE: u32 = 0o700;

/// The paths handed down to every crate of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildPaths {
    /// The workdir (`--workdir`): in kaniko mode the root being built.
    pub root: PathBuf,
    /// The state directory (`--state-dir`), which holds the store
    /// (ADR-0022).
    pub state: PathBuf,
}

impl BuildPaths {
    /// Paths from the workdir and the state directory, both absolute.
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
    /// Owner.
    pub uid: u32,
    /// Group.
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
    /// [`Store::close`] leaves its directories to the next start.
    _build_lock: OwnedFd,
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
    /// canonicalized; a new or empty directory gets the `.ranikust-root`
    /// marker, and a non-empty one without it is refused. A state directory
    /// owned by another user or writable by others is used with a warning.
    ///
    /// # Errors
    ///
    /// [`Error::NotAStateDir`] for a foreign directory, [`Error::NoFlock`]
    /// when its filesystem has no `flock`, [`Error::Io`] or
    /// [`Error::Rootfs`] when it cannot be created or opened (including
    /// [`rnk_rootfs::Error::Unsupported`] before Linux 5.8).
    pub fn open(paths: &BuildPaths) -> Result<Self> {
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
        check_marker(&state, &state_path)?;
        for d in DIRS {
            state.create_dir_all(d, DIR_MODE)?;
        }
        warn_if_shared(&state, &state_path);
        let (id, build_lock) = register(&state, &state_path)?;
        let store = Self {
            build_dir: state_path.join("builds").join(&id),
            state,
            state_path,
            id,
            _build_lock: build_lock,
            held: lock::InProcess::default(),
            cache_holds: Arc::default(),
            next_tmp: AtomicU64::new(0),
        };
        store.cleanup()?;
        Ok(store)
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
    /// completely). A blob is then `fsync`ed [open: Q-100] and the entry
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
        let _lock = self.lock(key)?;
        let rel = key.path();
        if let Some(size) = self.published(key)? {
            span.record("reused", true);
            span.record("bytes_written", 0);
            span.record("bytes_reused", size);
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
                span.record("reused", false);
                span.record("bytes_written", bytes);
                span.record("bytes_reused", 0);
                Ok(self.host(&rel))
            }
            Err(e) => {
                self.discard(&tmp);
                Err(e)
            }
        }
    }

    /// `caches/<sha256(id)>`, the directory of a `RUN --mount=type=cache`
    /// with this id, created if missing and kept between builds. Hashing
    /// keeps any id, `../..` included, a single plain name.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when it cannot be created.
    pub fn cache_dir(&self, id: &str) -> Result<PathBuf> {
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
                Some(rustix::fs::Uid::from_raw(owner.uid)),
                Some(rustix::fs::Gid::from_raw(owner.gid)),
            )
            .map_err(|e| self.io_err("chown", tmp, e.into()))?;
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
    /// # Errors
    ///
    /// [`Error::Rootfs`] or [`Error::BadEntry`] when a directory cannot be
    /// removed, for example because a mount is still inside it or on it;
    /// the next start retries.
    pub fn close(self) -> Result<()> {
        remove_build(&self.state, &self.id)?;
        Ok(())
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

/// Refuses a non-empty state directory without a regular-file marker, and
/// marks an empty one.
fn check_marker(state: &RootPath, state_path: &Path) -> Result<()> {
    let refuse = || Error::NotAStateDir {
        path: state_path.to_owned(),
    };
    let marked = |state: &RootPath| -> Result<Option<bool>> {
        match state.statx(MARKER) {
            Ok(st) => Ok(Some(st.file_type() == FileType::Regular)),
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    match marked(state)? {
        Some(true) => return Ok(()),
        Some(false) => return Err(refuse()),
        None => {}
    }
    if !list(state, Path::new("."))?.is_empty() {
        // A build creating the store concurrently writes the marker first.
        return match marked(state)? {
            Some(true) => Ok(()),
            _ => Err(refuse()),
        };
    }
    match state.open_at(MARKER, &OpenHow::write().create_new(0o644)) {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => match marked(state)? {
            Some(true) => Ok(()),
            _ => Err(refuse()),
        },
        Err(e) => Err(e.into()),
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
/// Not for `caches/`, which `RUN` writes, or anything else a build's
/// content can reach, until `rnk-rootfs` offers a deletion that opens the
/// top directory itself without following or crossing.
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
fn remove_dir_tree(state: &RootPath, rel: &Path, st: &Stat) -> Result<()> {
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
