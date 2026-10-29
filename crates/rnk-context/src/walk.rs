//! The context walk, as BuildKit's filtered walk does it.
//!
//! Mirrors `fsutil`'s `filterFS.Walk` with exclude patterns only (the
//! version BuildKit v0.34.0 uses, commit `83cac42c1c52`): entries come in
//! `filepath.Walk` order (depth first, names sorted byte-wise within a
//! directory, symlinks not followed); each path is judged with
//! `MatchesUsingParentResults`, chaining its parent's match state; an
//! excluded directory is skipped whole unless an exception may re-include
//! something below it, and is then reported only when something inside it
//! is (ADR-0027).

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rnk_ignore::{MatchInfo, Matcher};
use rnk_rootfs::{FileTime, FileType, OpenHow, RootPath};
use rustix::fs::{AtFlags, StatxFlags};

use crate::{Error, Result};

/// What the walk learned about an entry, without following a symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryMeta {
    /// The entry type.
    pub file_type: FileType,
    /// The raw `st_mode`: type and permission bits.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Size in bytes.
    pub size: u64,
    /// Last modification of the content.
    pub mtime: FileTime,
}

impl EntryMeta {
    fn from_statx(x: &rustix::fs::Statx) -> Self {
        let mode = u32::from(x.stx_mode);
        Self {
            file_type: file_type(mode),
            mode,
            uid: x.stx_uid,
            gid: x.stx_gid,
            size: x.stx_size,
            mtime: FileTime {
                secs: x.stx_mtime.tv_sec,
                nanos: x.stx_mtime.tv_nsec,
            },
        }
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

/// An entry the walk reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path relative to the context root, `/`-separated, never empty.
    pub path: PathBuf,
    /// Type and metadata.
    pub meta: EntryMeta,
}

/// Totals of a walk, also recorded on the `rnk_context.load` span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Reported entries that are not directories.
    pub files: u64,
    /// Reported directories.
    pub dirs: u64,
    /// Total size of the reported regular files.
    pub bytes: u64,
    /// Excluded entries that are not directories, plus excluded
    /// directories skipped whole. An excluded directory held back for a
    /// possible exception and never reported is not counted.
    pub files_ignored: u64,
}

/// An entry found but not judged yet.
struct Pending {
    path: PathBuf,
    meta: EntryMeta,
}

/// A directory the walk is inside: fsutil's `visitedDir`.
struct Visited {
    path_with_sep: Vec<u8>,
    info: MatchInfo,
    entry: Entry,
    reported: bool,
}

/// The walk over a context, an iterator of entries in BuildKit's order.
///
/// Created by [`Context::walk`](crate::Context::walk). Stops at the first
/// error. Never opens anything but directories, so FIFOs and device nodes
/// in a context are reported, not read.
pub struct Walk<'c> {
    root: &'c RootPath,
    matcher: Option<&'c Matcher>,
    pending: Vec<Pending>,
    parents: Vec<Visited>,
    ready: VecDeque<Entry>,
    stats: WalkStats,
    started: bool,
    failed: bool,
    /// An error returned after the entries already queued, so that a
    /// directory is reported before the error reading it, as in fsutil.
    deferred: Option<Error>,
    span: tracing::Span,
}

impl<'c> Walk<'c> {
    pub(crate) fn new(root: &'c RootPath, matcher: Option<&'c Matcher>) -> Self {
        let span = tracing::info_span!(
            "rnk_context.load",
            files = tracing::field::Empty,
            dirs = tracing::field::Empty,
            bytes = tracing::field::Empty,
            files_ignored = tracing::field::Empty,
        );
        Self {
            root,
            matcher,
            pending: Vec::new(),
            parents: Vec::new(),
            ready: VecDeque::new(),
            stats: WalkStats::default(),
            started: false,
            failed: false,
            deferred: None,
            span,
        }
    }

    /// The totals so far; final once the iterator returned `None`.
    pub fn stats(&self) -> WalkStats {
        self.stats
    }

    /// Reads the directory `rel` (the root when empty): its entries,
    /// sorted byte-wise, each with its `statx`.
    fn read_dir(&self, rel: &Path) -> Result<Vec<Pending>> {
        let dir_path = if rel.as_os_str().is_empty() {
            Path::new(".")
        } else {
            rel
        };
        // Resolved inside the root; a symlink in its place fails with
        // `ELOOP` instead of being followed.
        let fd = self
            .root
            .open_at(dir_path, &OpenHow::read().directory().no_follow())?;
        let walk_err = |source: rustix::io::Errno| Error::Walk {
            path: rel.to_owned(),
            source: source.into(),
        };
        let mut names = Vec::new();
        let mut dir = rustix::fs::Dir::read_from(&fd).map_err(walk_err)?;
        while let Some(entry) = dir.read() {
            let entry = entry.map_err(walk_err)?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_owned());
            }
        }
        names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        names
            .into_iter()
            .map(|name| {
                // A single component from `getdents`, relative to a directory
                // already confined to the root, never following a symlink:
                // it cannot reach outside the context.
                let x = rustix::fs::statx(
                    &fd,
                    name.as_os_str(),
                    AtFlags::SYMLINK_NOFOLLOW,
                    StatxFlags::BASIC_STATS,
                )
                .map_err(|source| Error::Walk {
                    path: rel.join(&name),
                    source: source.into(),
                })?;
                Ok(Pending {
                    path: rel.join(&name),
                    meta: EntryMeta::from_statx(&x),
                })
            })
            .collect()
    }

    fn push_children(&mut self, children: Vec<Pending>) {
        // Last pushed is visited first: keep the sorted order.
        self.pending.extend(children.into_iter().rev());
    }

    fn count(&mut self, entry: &Entry) {
        match entry.meta.file_type {
            FileType::Directory => self.stats.dirs += 1,
            FileType::Regular => {
                self.stats.files += 1;
                self.stats.bytes += entry.meta.size;
            }
            _ => self.stats.files += 1,
        }
    }

    /// Judges one entry: reports it (and the held-back directories above
    /// it), skips it, and reads it when it is a directory to descend into.
    fn visit(&mut self, p: Pending) -> Result<()> {
        let path_bytes = p.path.as_os_str().as_bytes().to_vec();
        while self
            .parents
            .last()
            .is_some_and(|v| !path_bytes.starts_with(&v.path_with_sep))
        {
            self.parents.pop();
        }
        let is_dir = p.meta.file_type == FileType::Directory;
        let (excluded, info) = match self.matcher {
            Some(m) => {
                let parent_info = self
                    .parents
                    .last()
                    .map(|v| v.info.clone())
                    .unwrap_or_default();
                m.matches_using_parent(&path_bytes, &parent_info)?
            }
            None => (false, MatchInfo::default()),
        };
        if excluded
            && is_dir
            && !self
                .matcher
                .is_some_and(|m| m.may_reinclude_below(&path_bytes))
        {
            self.stats.files_ignored += 1;
            return Ok(());
        }
        let entry = Entry {
            path: p.path,
            meta: p.meta,
        };
        if excluded {
            if !is_dir {
                self.stats.files_ignored += 1;
            }
        } else {
            // Directories held back while excluded are reported once
            // something inside them is, outermost first.
            for i in 0..self.parents.len() {
                if !self.parents[i].reported {
                    self.parents[i].reported = true;
                    let held = self.parents[i].entry.clone();
                    self.count(&held);
                    self.ready.push_back(held);
                }
            }
            self.count(&entry);
            self.ready.push_back(entry.clone());
        }
        if is_dir {
            let children = self.read_dir(&entry.path)?;
            let mut path_with_sep = path_bytes;
            path_with_sep.push(b'/');
            self.parents.push(Visited {
                path_with_sep,
                info,
                entry,
                reported: !excluded,
            });
            self.push_children(children);
        }
        Ok(())
    }
}

impl Iterator for Walk<'_> {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.ready.pop_front() {
                return Some(Ok(entry));
            }
            if self.failed {
                return None;
            }
            if let Some(e) = self.deferred.take() {
                self.failed = true;
                return Some(Err(e));
            }
            if !self.started {
                self.started = true;
                let span = self.span.clone();
                let _entered = span.enter();
                match self.read_dir(Path::new("")) {
                    Ok(children) => self.push_children(children),
                    Err(e) => self.deferred = Some(e),
                }
                continue;
            }
            let pending = self.pending.pop()?;
            let span = self.span.clone();
            let _entered = span.enter();
            if let Err(e) = self.visit(pending) {
                self.deferred = Some(e);
            }
        }
    }
}

impl Drop for Walk<'_> {
    fn drop(&mut self) {
        self.span.record("files", self.stats.files);
        self.span.record("dirs", self.stats.dirs);
        self.span.record("bytes", self.stats.bytes);
        self.span.record("files_ignored", self.stats.files_ignored);
    }
}
