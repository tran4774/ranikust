//! The kaniko-mode walk (`design/kaniko-mode.md#walk-hash-and-diff`,
//! ADR-0039): one `rayon` task per directory, each directory opened
//! relative to its parent's descriptor with `openat2(RESOLVE_BENEATH |
//! NO_SYMLINKS | NO_MAGICLINKS | NO_XDEV)` and read with `getdents64`,
//! `statx` per entry, entries on another mount skipped. Regular files are
//! hashed with xxh3-128, content and xattrs (`*.overlay.*` excluded), unless
//! the previous walk saw the same `statx` key outside the racy window (the
//! stat cache). The diff against the previous walk is computed while
//! walking: each entry is compared with its previous self, found by name in
//! the previous walk's path arena, and marked seen; unseen previous entries
//! are deletions.
//!
//! What is compared follows kaniko's `--snapshot-mode`
//! (`kaniko@44aa7ddc0 pkg/executor/build.go:2474-2486`): `full`, kaniko's
//! `Hasher` (`pkg/util/util.go:52-112`: mode, mtime, uid, gid, the
//! `security.capability` xattr and the content or link target), here
//! widened to size, rdev and every xattr but overlay's; `redo`, kaniko's
//! `RedoHasher` (`pkg/util/util.go:178-206`: mode, mtime, size, uid, gid);
//! `time`, kaniko's `MtimeHasher` (`pkg/util/util.go:164-176`: mtime). In
//! every mode a change of the entry's type counts as a change, which kaniko's
//! `time` mode misses when the mtime happens to match.

use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rnk_rootfs::{FileTime, FileType, IgnoreList, OpenHow, RootPath};
use rustix::fs::{
    AtFlags, Mode, OFlags, PROC_SUPER_MAGIC, RawDir, ResolveFlags, Statx, StatxAttributes,
    StatxFlags,
};
use rustix::io::Errno;
use xxhash_rust::xxh3::Xxh3;

use super::tree::{DirOut, ROOT, Tree};
use crate::error::{Error, Result};

/// The walk's default worker count: 16 and 32 workers were slower than 8 on
/// every hash measured (ADR-0039).
pub const DEFAULT_WALK_WORKERS: NonZeroUsize = match NonZeroUsize::new(8) {
    Some(n) => n,
    None => unreachable!(),
};

/// How long before the start of a walk a file's `mtime` and `ctime` must
/// both be for its hash to be reused by the next walk, as kaniko's
/// `StatHashCacheRacyWindow` (`kaniko@44aa7ddc0
/// pkg/util/stat_hash_cache.go:29-41`): timestamps are coarser than the
/// clock, so a file written twice within one tick keeps its key.
const RACY_WINDOW_SECS: i64 = 2;

/// The type bits of `st_mode`.
const S_IFMT: u16 = 0o170_000;

/// Bytes read per `read(2)` while hashing.
const READ_BUF: usize = 128 * 1024;

/// Bytes of `getdents64` buffer per directory task.
const DIR_BUF: usize = 32 * 1024;

/// How a directory is opened: by its name in its parent, never through a
/// symlink, a magic link or onto another mount.
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);

/// What the walk compares, kaniko's `--snapshot-mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SnapshotMode {
    /// Type, mode, owner, size, mtime, device number, content or link
    /// target, and xattrs.
    #[default]
    Full,
    /// Type, mode, mtime, size and owner; no file is read.
    Redo,
    /// Type and mtime only; no file is read.
    Time,
}

/// Settings of a [`Walker`].
#[derive(Clone, Copy, Debug)]
pub struct WalkOptions {
    /// Threads of the walk's pool (`--snapshot-workers`). It never changes
    /// the result.
    pub workers: NonZeroUsize,
    /// What is compared.
    pub mode: SnapshotMode,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            workers: DEFAULT_WALK_WORKERS,
            mode: SnapshotMode::Full,
        }
    }
}

/// An entry's metadata as the walk saw it, for the exporter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntryMeta {
    /// The entry type.
    pub kind: FileType,
    /// The raw `st_mode`: type and permission bits.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Size in bytes.
    pub size: u64,
    /// Last modification.
    pub mtime: FileTime,
    /// Major and minor number of a device node.
    pub rdev: (u32, u32),
    /// Major and minor number of the device holding the entry: with
    /// [`EntryMeta::ino`], which entries are hard links of one inode.
    pub dev: (u32, u32),
    /// Inode number.
    pub ino: u64,
    /// Number of hard links.
    pub nlink: u32,
}

/// Whether an entry is new or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Not in the previous walk, or there under another type.
    Added,
    /// In the previous walk, with something compared changed.
    Modified,
}

/// An added or modified entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    /// The path relative to the root, never empty (the root is
    /// [`WalkDiff::root`]).
    pub path: PathBuf,
    /// New or changed.
    pub change: Change,
    /// What the walk saw.
    pub meta: EntryMeta,
}

/// Counts of one walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Entries read from directories, skipped ones included, the root not
    /// included.
    pub entries: u64,
    /// Regular files read and hashed.
    pub files_hashed: u64,
    /// Bytes read for hashing.
    pub bytes_hashed: u64,
    /// Regular files whose hash came from the previous walk.
    pub stat_cache_hits: u64,
    /// Entries not walked: on the ignore list, on another mount or a mount
    /// root.
    pub skipped: u64,
    /// Threads of the pool that took part.
    pub max_threads: usize,
}

/// What changed in the root since the previous walk of the same
/// [`Walker`]; against nothing, everything is [`Change::Added`].
///
/// Both lists are sorted by path, component by component, so a directory
/// comes before what it holds; the order never depends on the worker count.
/// Entries on the ignore list, on another mount, and below them, are in
/// neither list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkDiff {
    /// The root directory itself, when its metadata changed (or on the
    /// first walk).
    pub root: Option<EntryMeta>,
    /// Added and modified entries. A directory is listed only when its own
    /// metadata changed; the exporter adds the parents of what it writes.
    pub changed: Vec<Changed>,
    /// Deleted paths, each the topmost one: a deleted directory is listed,
    /// not what it held. An entry replaced by one of another type is in
    /// [`WalkDiff::changed`] as [`Change::Added`], not here, and what a
    /// replaced directory held is not listed either: the new entry hides it.
    pub deleted: Vec<PathBuf>,
    /// Counts.
    pub stats: WalkStats,
}

/// One entry as the walk keeps it for the next walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    meta: EntryMeta,
    ctime: FileTime,
    /// xxh3-128 of a regular file's content or a symlink's target; 0
    /// otherwise, and when the mode reads no content.
    content: u128,
    /// xxh3-128 of the xattrs but overlay's (regular files and directories,
    /// `full` only); 0 for none.
    xattrs: u128,
    /// The hashes may be reused by the next walk: `mtime` and `ctime` were
    /// outside the racy window when this walk started.
    cacheable: bool,
}

impl Record {
    pub(crate) fn is_dir(&self) -> bool {
        self.meta.kind == FileType::Directory
    }

    /// What the stat cache compares: every `statx` field a change of the
    /// content or the xattrs also changes (`ctime`), as kaniko's `statKey`
    /// (`kaniko@44aa7ddc0 pkg/util/stat_hash_cache.go:44-61`).
    fn same_key(&self, other: &Self) -> bool {
        let (a, b) = (&self.meta, &other.meta);
        a.dev == b.dev
            && a.ino == b.ino
            && a.size == b.size
            && a.mtime == b.mtime
            && self.ctime == other.ctime
            && a.mode == b.mode
            && a.uid == b.uid
            && a.gid == b.gid
    }

    /// Whether `self` (now) differs from `old` (the previous walk) in what
    /// `mode` compares.
    fn differs(&self, old: &Self, mode: SnapshotMode) -> bool {
        let (a, b) = (&self.meta, &old.meta);
        if a.kind != b.kind || a.mtime != b.mtime {
            return true;
        }
        match mode {
            SnapshotMode::Time => false,
            SnapshotMode::Redo => {
                a.mode != b.mode || a.size != b.size || a.uid != b.uid || a.gid != b.gid
            }
            SnapshotMode::Full => {
                a.mode != b.mode
                    || a.size != b.size
                    || a.uid != b.uid
                    || a.gid != b.gid
                    || a.rdev != b.rdev
                    || self.content != old.content
                    || self.xattrs != old.xattrs
            }
        }
    }
}

/// States of a previous entry during a walk.
const UNSEEN: u8 = 0;
const SEEN_DIR: u8 = 1;
const SEEN_OTHER: u8 = 2;
/// Skipped (ignored, on another mount): neither compared nor deleted.
const KEPT: u8 = 3;

/// Walks a root again and again, each walk diffed against the previous one.
///
/// It owns a `rayon` pool of [`WalkOptions::workers`] threads, started by
/// [`Walker::new`]: create it after the process set-up, never before
/// `unshare` (`docs/agents/rules.md#code-traps`).
pub struct Walker {
    opts: WalkOptions,
    pool: rayon::ThreadPool,
    prev: Option<Tree>,
    racy_window_secs: i64,
}

impl std::fmt::Debug for Walker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walker")
            .field("opts", &self.opts)
            .field("entries", &self.entries())
            .finish_non_exhaustive()
    }
}

impl Walker {
    /// A walker with nothing walked yet.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when the threads cannot start.
    pub fn new(opts: WalkOptions) -> Result<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(opts.workers.get())
            .thread_name(|i| format!("rnk-walk-{i}"))
            .build()
            .map_err(|e| Error::Pool {
                workers: opts.workers.get(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            opts,
            pool,
            prev: None,
            racy_window_secs: RACY_WINDOW_SECS,
        })
    }

    /// The same walker with another racy window, so tests need not wait.
    #[cfg(test)]
    pub(crate) fn with_racy_window(mut self, secs: i64) -> Self {
        self.racy_window_secs = secs;
        self
    }

    /// The settings.
    #[must_use]
    pub fn options(&self) -> WalkOptions {
        self.opts
    }

    /// Entries the last walk kept for the next diff, the root not included.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.prev.as_ref().map_or(0, |t| t.len().saturating_sub(1))
    }

    /// Walks `root` and keeps the result for the next [`Walker::diff`],
    /// without computing a diff: a stage's starting point (kaniko's
    /// `snapshot.init`).
    ///
    /// # Errors
    ///
    /// As [`Walker::diff`].
    pub fn baseline(&mut self, root: &RootPath, ignore: &IgnoreList) -> Result<WalkStats> {
        let span = tracing::debug_span!("rnk_snapshot.baseline", entries = tracing::field::Empty);
        let _entered = span.enter();
        let stats = self.run(root, ignore, false)?.stats;
        span.record("entries", stats.entries);
        Ok(stats)
    }

    /// Walks `root`, returns what changed since the previous walk and keeps
    /// this walk for the next one.
    ///
    /// `root` must not change during the walk: nothing else may write to it.
    /// Entries on `ignore`, on another mount than the root's, and mount roots
    /// are skipped with everything below them.
    ///
    /// # Errors
    ///
    /// [`Error::Walk`] when a system call fails (an `EMFILE` asks for a
    /// higher `RLIMIT_NOFILE`); [`Error::Changed`] when an entry was swapped
    /// during the walk; [`Error::Rootfs`] when the root cannot be opened.
    /// After an error the previous walk is kept.
    pub fn diff(&mut self, root: &RootPath, ignore: &IgnoreList) -> Result<WalkDiff> {
        self.run(root, ignore, true)
    }

    /// Forgets the previous walk, as after emptying the root: the next diff
    /// lists everything as added.
    pub fn forget(&mut self) {
        self.prev = None;
    }

    fn run(&mut self, root: &RootPath, ignore: &IgnoreList, want: bool) -> Result<WalkDiff> {
        let span = tracing::debug_span!(
            "rnk_snapshot.walk",
            entries = tracing::field::Empty,
            files_hashed = tracing::field::Empty,
            bytes_hashed = tracing::field::Empty,
            stat_cache_hits = tracing::field::Empty,
            max_threads = tracing::field::Empty,
        );
        let _entered = span.enter();
        let start = now();
        let root_fd = root.open_at("/", &OpenHow::read().directory())?;
        let root_st = statx_at(root_fd.as_fd(), c"", AtFlags::EMPTY_PATH, Path::new(""))?;
        if root_st.stx_mnt_id != root.mnt_id() {
            return Err(Error::Changed {
                path: PathBuf::new(),
            });
        }
        // Regular files are pinned with `O_PATH`, checked, then reopened for
        // reading through ranikust's own `/proc/self/fd`, as
        // `rnk_rootfs::RootPath::open_regular`: only `full` reads files.
        let proc = match self.opts.mode {
            SnapshotMode::Full => Some(proc_fd_dir()?),
            SnapshotMode::Redo | SnapshotMode::Time => None,
        };
        let prev = self.prev.as_ref();
        let seen: Vec<AtomicU8> = (0..prev.map_or(0, Tree::len))
            .map(|_| AtomicU8::new(UNSEEN))
            .collect();
        if let Some(s) = seen.first() {
            s.store(SEEN_DIR, Ordering::Relaxed);
        }
        let ctx = Ctx {
            prev,
            proc,
            seen: &seen,
            ignore,
            root_mnt: root.mnt_id(),
            mode: self.opts.mode,
            racy_limit: FileTime {
                secs: start.secs - self.racy_window_secs,
                nanos: start.nanos,
            },
            want,
            dirs: Mutex::new(Vec::new()),
            changed: Mutex::new(Vec::new()),
            error: Mutex::new(None),
            failed: AtomicBool::new(false),
            entries: AtomicU64::new(0),
            files_hashed: AtomicU64::new(0),
            bytes_hashed: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            max_thread: AtomicUsize::new(0),
        };
        let mut root_rec = ctx.record(&root_st);
        if self.opts.mode == SnapshotMode::Full {
            root_rec.xattrs = xattr_hash(root_fd.as_fd(), Path::new(""))?;
        }
        let prev_root = prev.map(|_| ROOT);
        self.pool
            .in_place_scope(|s| ctx.dir_task(s, root_fd, Vec::new(), prev_root));
        let Ctx {
            dirs,
            changed,
            error,
            entries,
            files_hashed,
            bytes_hashed,
            cache_hits,
            skipped,
            max_thread,
            ..
        } = ctx;
        if let Some(e) = into_inner(error) {
            return Err(e);
        }
        let mut changed = into_inner(changed);
        changed.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut deleted = Vec::new();
        if want && let Some(p) = prev {
            for (state, id) in seen.iter().zip(0_u32..).skip(1) {
                let parent = seen
                    .get(p.parent(id) as usize)
                    .map(|s| s.load(Ordering::Relaxed));
                if state.load(Ordering::Relaxed) == UNSEEN && parent == Some(SEEN_DIR) {
                    deleted.push(p.path(id));
                }
            }
            deleted.sort_unstable();
        }
        let root_changed = match prev {
            Some(p) => root_rec.differs(&p.node(ROOT).rec, self.opts.mode),
            None => true,
        };
        let stats = WalkStats {
            entries: entries.into_inner(),
            files_hashed: files_hashed.into_inner(),
            bytes_hashed: bytes_hashed.into_inner(),
            stat_cache_hits: cache_hits.into_inner(),
            skipped: skipped.into_inner(),
            max_threads: max_thread.into_inner(),
        };
        span.record("entries", stats.entries);
        span.record("files_hashed", stats.files_hashed);
        span.record("bytes_hashed", stats.bytes_hashed);
        span.record("stat_cache_hits", stats.stat_cache_hits);
        span.record("max_threads", stats.max_threads);
        let root_meta = root_rec.meta;
        drop(seen);
        self.prev = Some(Tree::build(root_rec, into_inner(dirs))?);
        Ok(WalkDiff {
            root: (want && root_changed).then_some(root_meta),
            changed: if want { changed } else { Vec::new() },
            deleted,
            stats,
        })
    }
}

/// A mutex's value; a panicked worker aborts the release build, so a
/// poisoned lock only carries what was there.
fn into_inner<T>(m: Mutex<T>) -> T {
    m.into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What every task of one walk shares.
struct Ctx<'a> {
    prev: Option<&'a Tree>,
    /// `/proc/self/fd` (`O_PATH`), in `full` mode.
    proc: Option<OwnedFd>,
    seen: &'a [AtomicU8],
    ignore: &'a IgnoreList,
    root_mnt: u64,
    mode: SnapshotMode,
    racy_limit: FileTime,
    want: bool,
    dirs: Mutex<Vec<DirOut>>,
    changed: Mutex<Vec<Changed>>,
    error: Mutex<Option<Error>>,
    failed: AtomicBool,
    entries: AtomicU64,
    files_hashed: AtomicU64,
    bytes_hashed: AtomicU64,
    cache_hits: AtomicU64,
    skipped: AtomicU64,
    max_thread: AtomicUsize,
}

thread_local! {
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl<'a> Ctx<'a> {
    fn fail(&self, e: Error) {
        self.failed.store(true, Ordering::Relaxed);
        let mut slot = self
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(e);
        }
    }

    fn mark(&self, prev: Option<u32>, state: u8) {
        if let Some(s) = prev.and_then(|id| self.seen.get(id as usize)) {
            s.store(state, Ordering::Relaxed);
        }
    }

    fn prev_rec(&self, id: Option<u32>) -> Option<&'a Record> {
        Some(&self.prev?.node(id?).rec)
    }

    /// A record from `statx`, hashes still 0.
    fn record(&self, st: &Statx) -> Record {
        let t = |ts: rustix::fs::StatxTimestamp| FileTime {
            secs: ts.tv_sec,
            nanos: ts.tv_nsec,
        };
        let mode = u32::from(st.stx_mode);
        let meta = EntryMeta {
            kind: file_type(mode),
            mode,
            uid: st.stx_uid,
            gid: st.stx_gid,
            size: st.stx_size,
            mtime: t(st.stx_mtime),
            rdev: (st.stx_rdev_major, st.stx_rdev_minor),
            dev: (st.stx_dev_major, st.stx_dev_minor),
            ino: st.stx_ino,
            nlink: st.stx_nlink,
        };
        let ctime = t(st.stx_ctime);
        Record {
            cacheable: meta.mtime < self.racy_limit && ctime < self.racy_limit,
            meta,
            ctime,
            content: 0,
            xattrs: 0,
        }
    }

    /// Reads one directory, then handles its entries; subdirectories become
    /// tasks of their own. `path` is the directory's path relative to the
    /// root, `prev` its node in the previous walk when it was a directory
    /// there.
    fn dir_task<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        dir: OwnedFd,
        path: Vec<u8>,
        prev: Option<u32>,
    ) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        if let Some(i) = rayon::current_thread_index() {
            self.max_thread.fetch_max(i + 1, Ordering::Relaxed);
        }
        if let Err(e) = self.dir_entries(s, dir, path, prev) {
            self.fail(e);
        }
    }

    /// A subdirectory's task: opens it in its parent, here rather than in
    /// the parent's task, so a directory waiting for a worker holds no
    /// descriptor of its own (only its parent's, shared), checks it is the
    /// directory the parent's `statx` saw, then walks it.
    fn child_task<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        parent: Arc<OwnedFd>,
        name: Vec<u8>,
        st: Statx,
        path: Vec<u8>,
        prev: Option<u32>,
    ) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        let opened = open_dir(parent.as_fd(), &name, &st, as_path(&path));
        drop(parent);
        match opened {
            Ok(dir) => self.dir_task(s, dir, path, prev),
            Err(e) => self.fail(e),
        }
    }

    fn dir_entries<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        dir: OwnedFd,
        path: Vec<u8>,
        prev: Option<u32>,
    ) -> Result<()> {
        let names = read_names(dir.as_fd(), &path)?;
        let dir = Arc::new(dir);
        let mut out = DirOut {
            path,
            ..DirOut::default()
        };
        let mut changed = Vec::new();
        let mut child_path = Vec::new();
        let mut walked = 0_u64;
        for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            walked += 1;
            child_path.clear();
            child_path.extend_from_slice(&out.path);
            if !child_path.is_empty() {
                child_path.push(b'/');
            }
            child_path.extend_from_slice(name);
            let prev_child = match (self.prev, prev) {
                (Some(t), Some(d)) => t.child(d, name),
                _ => None,
            };
            // `covers`, not `contains`: `--ignore-path` globs and kaniko's
            // below-only entries match names that are not literal entries.
            if !self.ignore.is_empty() && self.ignore.covers(as_path(&child_path)) {
                self.mark(prev_child, KEPT);
                self.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let Some(rec) = self.entry(s, &dir, name, &child_path, prev_child)? else {
                continue;
            };
            if self.want {
                let change = match self.prev_rec(prev_child) {
                    None => Some(Change::Added),
                    Some(old) if old.meta.kind != rec.meta.kind => Some(Change::Added),
                    Some(old) if rec.differs(old, self.mode) => Some(Change::Modified),
                    Some(_) => None,
                };
                if let Some(change) = change {
                    changed.push(Changed {
                        path: as_path(&child_path).to_path_buf(),
                        change,
                        meta: rec.meta,
                    });
                }
            }
            self.mark(prev_child, if rec.is_dir() { SEEN_DIR } else { SEEN_OTHER });
            out.push(name, rec)?;
        }
        self.entries.fetch_add(walked, Ordering::Relaxed);
        if !changed.is_empty() {
            self.changed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(changed);
        }
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(out);
        Ok(())
    }

    /// One entry of `dir`: its record, or `None` when it is skipped or gone.
    /// A directory is opened here and walked by a task of its own.
    fn entry<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        parent: &Arc<OwnedFd>,
        name: &[u8],
        path: &[u8],
        prev: Option<u32>,
    ) -> Result<Option<Record>> {
        let shown = as_path(path);
        let dir = parent.as_fd();
        let st = match statx_at(
            dir,
            name,
            AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
            shown,
        ) {
            Ok(st) => st,
            // Removed since `getdents64`: not in this walk.
            Err(Error::Walk {
                errno: Errno::NOENT,
                ..
            }) => return Ok(None),
            Err(e) => return Err(e),
        };
        if st.stx_mnt_id != self.root_mnt || st.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
        {
            self.mark(prev, KEPT);
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        let old = self.prev_rec(prev);
        let mut rec = self.record(&st);
        match rec.meta.kind {
            FileType::Directory => {
                if self.mode == SnapshotMode::Full {
                    // Opened only for its xattrs, and closed again: the
                    // walk of its entries opens it in its own task.
                    let fd = open_dir(dir, name, &st, shown)?;
                    rec.xattrs = xattr_hash(fd.as_fd(), shown)?;
                }
                let prev_dir = old.filter(|o| o.is_dir()).and(prev);
                let (parent, name, child_path) = (Arc::clone(parent), name.to_vec(), path.to_vec());
                s.spawn(move |s| self.child_task(s, parent, name, st, child_path, prev_dir));
            }
            FileType::Regular if self.mode == SnapshotMode::Full => {
                match old.filter(|o| o.cacheable && o.meta.kind == FileType::Regular) {
                    Some(o) if o.same_key(&rec) => {
                        rec.content = o.content;
                        rec.xattrs = o.xattrs;
                        self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        let Some(fd) = self.open_file(dir, name, &st, shown)? else {
                            return Ok(None);
                        };
                        let (hash, read) = hash_content(fd.as_fd(), st.stx_size, shown)?;
                        rec.content = hash;
                        rec.xattrs = xattr_hash(fd.as_fd(), shown)?;
                        self.files_hashed.fetch_add(1, Ordering::Relaxed);
                        self.bytes_hashed.fetch_add(read, Ordering::Relaxed);
                    }
                }
            }
            FileType::Symlink if self.mode == SnapshotMode::Full => {
                let target = match rustix::fs::readlinkat(dir, name, Vec::new()) {
                    Ok(t) => t,
                    Err(Errno::NOENT) => return Ok(None),
                    Err(errno) => return Err(walk_err("readlinkat", shown, errno)),
                };
                rec.content = xxhash_rust::xxh3::xxh3_128(target.as_bytes());
            }
            FileType::Regular
            | FileType::Symlink
            | FileType::CharDevice
            | FileType::BlockDevice
            | FileType::Fifo
            | FileType::Socket
            | FileType::Unknown => {}
        }
        Ok(Some(rec))
    }

    /// Opens the regular file `name` of `dir`, which `statx` saw as `st`,
    /// for hashing. It is pinned with `O_PATH` first, which opens nothing,
    /// and checked to be that same regular file; only then is the pinned
    /// inode reopened for reading through `/proc/self/fd`, so a FIFO or a
    /// device node swapped in meanwhile is never opened, and no driver's
    /// `open` runs as root. `O_NOATIME` where the caller owns the file.
    /// `None` when it is gone.
    fn open_file(
        &self,
        dir: BorrowedFd<'_>,
        name: &[u8],
        st: &Statx,
        shown: &Path,
    ) -> Result<Option<OwnedFd>> {
        let Some(proc) = &self.proc else {
            return Err(walk_err("open", shown, Errno::NOTSUP));
        };
        open_regular(proc.as_fd(), dir, name, st, shown)
    }
}

/// [`Ctx::open_file`] with `/proc/self/fd` given.
fn open_regular(
    proc: BorrowedFd<'_>,
    dir: BorrowedFd<'_>,
    name: &[u8],
    st: &Statx,
    shown: &Path,
) -> Result<Option<OwnedFd>> {
    let pinned = match open_beneath(dir, name, OFlags::PATH, shown) {
        Ok(fd) => fd,
        Err(Error::Walk {
            errno: Errno::NOENT,
            ..
        }) => return Ok(None),
        Err(e) => return Err(e),
    };
    same_inode(pinned.as_fd(), st, shown)?;
    reopen_regular(proc, pinned.as_fd(), shown).map(Some)
}

fn now() -> FileTime {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    FileTime {
        secs: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        nanos: d.subsec_nanos(),
    }
}

fn as_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

fn walk_err(op: &'static str, path: &Path, errno: Errno) -> Error {
    Error::Walk {
        op,
        path: path.to_path_buf(),
        errno,
    }
}

fn file_type(mode: u32) -> FileType {
    use rustix::fs::FileType as Raw;
    match Raw::from_raw_mode(mode) {
        Raw::RegularFile => FileType::Regular,
        Raw::Directory => FileType::Directory,
        Raw::Symlink => FileType::Symlink,
        Raw::CharacterDevice => FileType::CharDevice,
        Raw::BlockDevice => FileType::BlockDevice,
        Raw::Fifo => FileType::Fifo,
        Raw::Socket => FileType::Socket,
        Raw::Unknown => FileType::Unknown,
    }
}

fn statx_at(
    dir: BorrowedFd<'_>,
    name: impl rustix::path::Arg,
    flags: AtFlags,
    shown: &Path,
) -> Result<Statx> {
    rustix::fs::statx(
        dir,
        name,
        flags,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    )
    .map_err(|e| walk_err("statx", shown, e))
}

/// Opens `name` in `dir` without leaving it, following no link and
/// crossing no mount; always `O_CLOEXEC | O_NOFOLLOW`.
fn open_beneath(dir: BorrowedFd<'_>, name: &[u8], flags: OFlags, shown: &Path) -> Result<OwnedFd> {
    let flags = flags | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    loop {
        match rustix::fs::openat2(dir, name, flags, Mode::empty(), RESOLVE) {
            Ok(fd) => return Ok(fd),
            // `openat2` asks to retry when a rename raced the lookup.
            Err(Errno::AGAIN | Errno::INTR) => {}
            Err(e) => return Err(walk_err("openat2", shown, e)),
        }
    }
}

/// `/proc/self/fd`, checked to be procfs, for [`reopen_regular`].
fn proc_fd_dir() -> Result<OwnedFd> {
    let shown = Path::new("/proc/self/fd");
    let fd = rustix::fs::open(
        shown,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| walk_err("open", shown, e))?;
    let fs = rustix::fs::fstatfs(&fd).map_err(|e| walk_err("fstatfs", shown, e))?;
    if fs.f_type != PROC_SUPER_MAGIC {
        return Err(walk_err("fstatfs", shown, Errno::NOTSUP));
    }
    Ok(fd)
}

/// Reopens the regular file pinned by the `O_PATH` fd `pinned` for
/// reading: exactly that inode, whatever its name points to now.
fn reopen_regular(proc: BorrowedFd<'_>, pinned: BorrowedFd<'_>, shown: &Path) -> Result<OwnedFd> {
    let num = pinned.as_raw_fd().to_string();
    let base = OFlags::RDONLY | OFlags::NOCTTY | OFlags::CLOEXEC;
    match rustix::fs::openat(proc, num.as_str(), base | OFlags::NOATIME, Mode::empty()) {
        // `O_NOATIME` needs ownership or `CAP_FOWNER`.
        Err(Errno::PERM) => rustix::fs::openat(proc, num.as_str(), base, Mode::empty()),
        r => r,
    }
    .map_err(|e| walk_err("reopen", shown, e))
}

/// Opens the directory `name` of `dir`, which `statx` saw as `st`, for
/// reading: `O_DIRECTORY` refuses anything else before opening it. Gone,
/// replaced or covered by a mount since `statx` is [`Error::Changed`]:
/// the root must not change during a walk.
fn open_dir(dir: BorrowedFd<'_>, name: &[u8], st: &Statx, shown: &Path) -> Result<OwnedFd> {
    let fd = match open_beneath(dir, name, OFlags::RDONLY | OFlags::DIRECTORY, shown) {
        Ok(fd) => fd,
        Err(Error::Walk {
            errno: Errno::NOENT | Errno::XDEV | Errno::NOTDIR | Errno::LOOP,
            ..
        }) => {
            return Err(Error::Changed {
                path: shown.to_path_buf(),
            });
        }
        Err(e) => return Err(e),
    };
    same_inode(fd.as_fd(), st, shown)?;
    Ok(fd)
}

/// The opened entry is the one `statx` saw: same type, inode, device and
/// mount.
fn same_inode(fd: BorrowedFd<'_>, st: &Statx, shown: &Path) -> Result<()> {
    let now = statx_at(fd, c"", AtFlags::EMPTY_PATH, shown)?;
    if (
        now.stx_mode & S_IFMT,
        now.stx_ino,
        now.stx_dev_major,
        now.stx_dev_minor,
        now.stx_mnt_id,
    ) == (
        st.stx_mode & S_IFMT,
        st.stx_ino,
        st.stx_dev_major,
        st.stx_dev_minor,
        st.stx_mnt_id,
    ) {
        Ok(())
    } else {
        Err(Error::Changed {
            path: shown.to_path_buf(),
        })
    }
}

/// The names in `dir`, each followed by a NUL, `.` and `..` left out.
fn read_names(dir: BorrowedFd<'_>, path: &[u8]) -> Result<Vec<u8>> {
    let mut buf = vec![MaybeUninit::<u8>::uninit(); DIR_BUF];
    let mut iter = RawDir::new(dir, &mut buf);
    let mut names = Vec::new();
    while let Some(entry) = iter.next() {
        let entry = entry.map_err(|e| walk_err("getdents64", as_path(path), e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.extend_from_slice(name);
            names.push(0);
        }
    }
    Ok(names)
}

/// xxh3-128 of the content of `fd`, whose `statx` size was `size`, and
/// how many bytes that was. At most `size + 1` bytes are read: a file
/// longer or shorter than `statx` said is changing, which is an error, so
/// a file that never ends cannot hold the walk.
fn hash_content(fd: BorrowedFd<'_>, size: u64, shown: &Path) -> Result<(u128, u64)> {
    let changed = || Error::Changed {
        path: shown.to_path_buf(),
    };
    READ.with_borrow_mut(|buf| {
        if buf.len() < READ_BUF {
            buf.resize(READ_BUF, 0);
        }
        let mut h = Xxh3::new();
        let mut total = 0_u64;
        loop {
            let room = size.saturating_add(1).saturating_sub(total);
            if room == 0 {
                return Err(changed());
            }
            let want = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
            match rustix::io::read(fd, buf.get_mut(..want).unwrap_or_default()) {
                Ok(0) => break,
                Ok(n) => {
                    h.update(buf.get(..n).unwrap_or_default());
                    total += u64::try_from(n).unwrap_or(u64::MAX);
                }
                Err(Errno::INTR) => {}
                Err(e) => return Err(walk_err("read", shown, e)),
            }
        }
        if total != size {
            return Err(changed());
        }
        Ok((h.digest128(), total))
    })
}

/// xxh3-128 of the xattrs of `fd`, overlay's (`*.overlay.*`) left out,
/// in name order; 0 when there are none. A filesystem without xattrs has
/// none.
fn xattr_hash(fd: BorrowedFd<'_>, shown: &Path) -> Result<u128> {
    let list = loop {
        let size = match rustix::fs::flistxattr_query(fd) {
            Ok(0) | Err(Errno::NOTSUP) => return Ok(0),
            Ok(n) => n,
            Err(e) => return Err(walk_err("flistxattr", shown, e)),
        };
        let mut list = vec![0_u8; size];
        match rustix::fs::flistxattr(fd, &mut list[..]) {
            Ok(n) => {
                list.truncate(n);
                break list;
            }
            // Grown since the query.
            Err(Errno::RANGE) => {}
            Err(e) => return Err(walk_err("flistxattr", shown, e)),
        }
    };
    let mut names: Vec<&[u8]> = list
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty() && !is_overlay(n))
        .collect();
    if names.is_empty() {
        return Ok(0);
    }
    names.sort_unstable();
    let mut h = Xxh3::new();
    for name in names {
        let value = loop {
            let size = match rustix::fs::fgetxattr_query(fd, name) {
                Ok(n) => n,
                // Removed since the list.
                Err(Errno::NODATA) => break None,
                Err(e) => return Err(walk_err("fgetxattr", shown, e)),
            };
            let mut value = vec![0_u8; size];
            match rustix::fs::fgetxattr(fd, name, &mut value[..]) {
                Ok(n) => {
                    value.truncate(n);
                    break Some(value);
                }
                Err(Errno::RANGE) => {}
                Err(Errno::NODATA) => break None,
                Err(e) => return Err(walk_err("fgetxattr", shown, e)),
            }
        };
        let Some(value) = value else { continue };
        h.update(name);
        h.update(&[0]);
        h.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
        h.update(&value);
    }
    Ok(h.digest128())
}

/// Overlay's own xattrs, `trusted.overlay.*` and `user.overlay.*`
/// (`design/kaniko-mode.md#walk-hash-and-diff`).
fn is_overlay(name: &[u8]) -> bool {
    name.windows(b".overlay.".len()).any(|w| w == b".overlay.")
}

#[cfg(test)]
mod tests;
