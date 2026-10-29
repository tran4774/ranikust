//! The store: opening and clean-up, per-digest locks, publication.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use rnk_oci::Digest;
use rnk_rootfs::{FileType, IgnoreList, OpenHow, RootPath, delete_contents};
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
/// Mode of the store's own directories: image content stays away from
/// other users of the host.
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
    /// The entry's host path, for callers that must reopen it (async I/O).
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
    next_tmp: AtomicU64,
}

impl Store {
    /// Opens the store in `paths.state`, creating it if needed, registers
    /// this build and removes what dead builds left in `tmp/` and
    /// `builds/`.
    ///
    /// The state directory is created if missing and canonicalized; a new
    /// or empty directory gets the `.ranikust-root` marker, and a non-empty
    /// one without it is refused.
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
        fs::create_dir_all(&paths.state).map_err(|e| host_err("create", e))?;
        let state_path = fs::canonicalize(&paths.state).map_err(|e| host_err("resolve", e))?;
        let state = RootPath::open(&state_path)?;
        check_marker(&state, &state_path)?;
        for d in DIRS {
            state.create_dir_all(d, DIR_MODE)?;
        }
        let (id, build_lock) = register(&state, &state_path)?;
        let store = Self {
            build_dir: state_path.join("builds").join(&id),
            state,
            state_path,
            id,
            _build_lock: build_lock,
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
    /// [`Error::Rootfs`] when the entry cannot be looked up.
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
    /// nothing is published. [`Error::Rootfs`] or [`Error::Io`] when the
    /// store cannot lock, create or rename.
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

    /// Ends the build: removes `builds/<build-id>` and `tmp/<build-id>`,
    /// then releases the build's lock.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when a directory cannot be removed, for example
    /// because a mount is still inside it; the next start retries.
    pub fn close(self) -> Result<()> {
        remove_build(&self.state, &self.id)?;
        Ok(())
    }

    /// Removes the `tmp/` and `builds/` directories of every build that no
    /// longer holds its lock. A directory that cannot be removed is left
    /// with a warning: it must not stop this build.
    fn cleanup(&self) -> Result<()> {
        let span = tracing::info_span!("rnk_store.cleanup", entries_removed = Empty);
        let _entered = span.enter();
        let mut ids = BTreeSet::new();
        for name in self.list("builds")? {
            let lock = name.as_encoded_bytes().ends_with(b".lock");
            if !lock {
                ids.insert(name);
            }
        }
        ids.extend(self.list("tmp")?);
        ids.remove(&OsString::from(&self.id));
        let mut removed = 0_u64;
        for id in ids {
            let Some(id) = id.to_str() else {
                tracing::warn!(target: "rnk_store", "state directory: skipping a non-UTF-8 build id {id:?}");
                continue;
            };
            let lock_rel = lock_file(id);
            let fd = lock::open(&self.state, &lock_rel)?;
            if !lock::try_lock(&fd).map_err(|e| self.io_err("lock", &lock_rel, e))? {
                continue; // a running build
            }
            match remove_build(&self.state, id) {
                Ok(n) => removed += n,
                Err(e) => {
                    tracing::warn!(target: "rnk_store", "state directory: leftovers of build {id} not removed: {e}");
                }
            }
        }
        span.record("entries_removed", removed);
        Ok(())
    }

    /// Takes the per-digest lock of `key`, waiting for it.
    fn lock(&self, key: &EntryKey) -> Result<OwnedFd> {
        let span = tracing::info_span!("rnk_store.lock", contended = Empty);
        let _entered = span.enter();
        let rel = key.lock_path();
        if let Some(parent) = rel.parent() {
            self.state.create_dir_all(parent, DIR_MODE)?;
        }
        let fd = lock::open(&self.state, &rel)?;
        let free = lock::try_lock(&fd).map_err(|e| self.io_err("lock", &rel, e))?;
        if !free {
            lock::lock(&fd).map_err(|e| self.io_err("lock", &rel, e))?;
        }
        span.record("contended", !free);
        Ok(fd)
    }

    /// The size of the published entry `key` (0 for a layer), or `None`.
    fn published(&self, key: &EntryKey) -> Result<Option<u64>> {
        match self.state.statx(key.path()) {
            Ok(st) => Ok(Some(match key {
                EntryKey::Blob(_) => st.size,
                EntryKey::Layer(_) => 0,
            })),
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(None),
            Err(e) => Err(e.into()),
        }
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

    /// The names in the store directory `rel`, `.` and `..` excluded.
    fn list(&self, rel: &str) -> Result<Vec<OsString>> {
        let fd = self
            .state
            .open_at(rel, &OpenHow::read().directory().no_follow())?;
        let err = |e: Errno| self.io_err("read directory", Path::new(rel), e.into());
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&fd).map_err(err)? {
            let name = entry.map_err(err)?.file_name().to_bytes().to_vec();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name));
            }
        }
        Ok(names)
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
    let mut entries = fs::read_dir(state_path).map_err(|source| Error::Io {
        op: "read directory",
        path: state_path.to_owned(),
        source,
    })?;
    if entries.next().is_some() {
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
                return Err(Error::NoFlock {
                    path: state_path.to_owned(),
                    source,
                });
            }
        }
        state.create_dir(Path::new("builds").join(&id), DIR_MODE)?;
        state.create_dir(Path::new("tmp").join(&id), DIR_MODE)?;
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
/// through `rnk-rootfs`: no symlink followed, no mount crossed. `false` when
/// it did not exist.
fn remove_tree(state: &RootPath, rel: &Path) -> Result<bool> {
    let st = match state.statx(rel) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if st.file_type() == FileType::Directory {
        let sub = state.sub_root(rel)?;
        delete_contents(&sub, &IgnoreList::new())?;
        drop(sub);
        state.remove_dir(rel)?;
    } else {
        state.remove_file(rel)?;
    }
    Ok(true)
}
