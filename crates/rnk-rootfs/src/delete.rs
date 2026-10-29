//! Deleting trees inside a root through file descriptors: never following a
//! symlink, never crossing a mount (compared by mount id, never `st_dev`),
//! stopping when an entry changes under it.
//!
//! This is the mechanism only; `rnk-snapshot` and `rnk-store` decide what to
//! delete and when.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};
use crate::meta::{self, FileType, Stat};
use crate::path;
use crate::root::{RootPath, open_name};

/// Paths inside a root that [`delete_contents`] keeps, with everything below
/// them. Matching is exact, on lexically cleaned paths: `/a/./b/` keeps
/// `a/b`, not `a/bc`. Their ancestors survive because they are not empty.
#[derive(Clone, Debug, Default)]
pub struct IgnoreList {
    paths: HashSet<PathBuf>,
}

impl IgnoreList {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `p`, a path inside the root, absolute or relative.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] when `p` names the root itself or climbs above
    /// it with `..`.
    pub fn insert(&mut self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        match path::clean(p) {
            Some(c) if !c.as_os_str().is_empty() => {
                self.paths.insert(c);
                Ok(())
            }
            Some(_) => Err(Error::InvalidPath {
                path: p.to_owned(),
                reason: "an ignore-list entry cannot be the root itself",
            }),
            None => Err(Error::InvalidPath {
                path: p.to_owned(),
                reason: "an ignore-list entry cannot leave the root",
            }),
        }
    }

    /// Whether `p` (cleaned the same way) is on the list.
    #[must_use]
    pub fn contains(&self, p: impl AsRef<Path>) -> bool {
        path::clean(p.as_ref()).is_some_and(|c| self.paths.contains(&c))
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Deletes everything inside `root`, keeping the root directory itself, the
/// entries on `keep` (and their ancestors), every entry on another mount and
/// every mount root.
///
/// Each directory is opened by `(parent fd, name)` with
/// `O_DIRECTORY | O_NOFOLLOW` and `RESOLVE_NO_XDEV`, then its `ino`, device
/// and mount id are compared with the `statx` taken before opening it; any
/// difference stops the deletion with [`Error::Changed`]. Symlinks are
/// unlinked, never followed. Depth is unlimited by `PATH_MAX`, but one fd is
/// held per level: the caller raises `RLIMIT_NOFILE` for deep trees.
///
/// # Errors
///
/// [`Error::Changed`] on a swap; [`Error::Io`] on the first failing
/// syscall (a permission error included). There is no path-based fallback:
/// whatever is left stays.
pub fn delete_contents(root: &RootPath, keep: &IgnoreList) -> Result<()> {
    let span = tracing::info_span!(
        "rnk_rootfs.delete_contents",
        files = tracing::field::Empty,
        dirs = tracing::field::Empty,
        kept = tracing::field::Empty,
    );
    let _entered = span.enter();
    let mut counts = Counts::default();
    let mut decide = |p: &Path, _: FileType| {
        if keep.paths.contains(p) {
            Keep::Whole
        } else {
            Keep::Remove
        }
    };
    let r = remove_below(root, Path::new(""), &mut decide, &mut counts);
    counts.record(&span);
    r.map(drop)
}

/// What to do with an entry met while deleting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// Delete it, and everything below it.
    Remove,
    /// Keep it and everything below it.
    Whole,
    /// Keep the directory itself but decide again for each entry in it; a
    /// non-directory is kept.
    Descend,
}

/// Counters for the tracing span of a deletion or extraction.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    pub(crate) files: u64,
    pub(crate) dirs: u64,
    pub(crate) kept: u64,
}

impl Counts {
    fn record(&self, span: &tracing::Span) {
        span.record("files", self.files);
        span.record("dirs", self.dirs);
        span.record("kept", self.kept);
    }
}

/// One directory being emptied.
struct Frame {
    /// `O_RDONLY | O_DIRECTORY` fd.
    fd: OwnedFd,
    /// Its path inside the root, cleaned and relative (`""` for the root).
    path: PathBuf,
    /// Its name in the parent frame.
    name: OsString,
    /// Entries still to visit.
    names: Vec<OsString>,
    /// Something below was kept, so the directory cannot be removed.
    kept: bool,
    /// The directory itself is kept ([`Keep::Descend`] or the start).
    keep_self: bool,
}

/// Deletes the entries below the directory `dir` (a cleaned path inside
/// `root`, `""` for the root) as `decide` says; `dir` itself stays. Returns
/// whether nothing below `dir` was kept.
///
/// `decide` gets each entry's cleaned path and type; entries on another
/// mount, and mount roots, are kept without asking.
pub(crate) fn remove_below(
    root: &RootPath,
    dir: &Path,
    decide: &mut dyn FnMut(&Path, FileType) -> Keep,
    counts: &mut Counts,
) -> Result<bool> {
    let fd = root.resolve(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let st = meta::statx_at(fd.as_fd(), OsStr::new(""), dir)?;
    if st.mnt_id != root.mnt_id() {
        counts.kept += 1;
        return Ok(false);
    }
    let names = read_names(fd.as_fd(), dir)?;
    let mut stack = vec![Frame {
        fd,
        path: dir.to_owned(),
        name: OsString::new(),
        names,
        kept: false,
        keep_self: true,
    }];

    loop {
        let Some(top) = stack.last_mut() else {
            // Only reached if the start frame was popped, which returns.
            return Ok(true);
        };
        let Some(name) = top.names.pop() else {
            let Some(done) = stack.pop() else {
                return Ok(true);
            };
            let Some(parent) = stack.last_mut() else {
                return Ok(!done.kept);
            };
            if done.kept || done.keep_self {
                parent.kept = true;
            } else {
                match rustix::fs::unlinkat(&parent.fd, &done.name, AtFlags::REMOVEDIR) {
                    Ok(()) => counts.dirs += 1,
                    // The name now holds something else than the directory
                    // just emptied (a swap), or something was added to it.
                    Err(Errno::NOTDIR | Errno::NOTEMPTY) => {
                        return Err(Error::Changed { path: done.path });
                    }
                    Err(e) => return Err(io_err("rmdir", &done.path, e)),
                }
            }
            continue;
        };
        let child = top.path.join(&name);
        let st = match meta::statx_at(top.fd.as_fd(), &name, &child) {
            Ok(st) => st,
            // Gone already: nothing to delete.
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => continue,
            Err(e) => return Err(e),
        };
        if st.mnt_id != root.mnt_id() || st.mount_root {
            top.kept = true;
            counts.kept += 1;
            continue;
        }
        let file_type = st.file_type();
        let action = decide(&child, file_type);
        match (action, file_type == FileType::Directory) {
            (Keep::Whole, _) | (Keep::Descend, false) => {
                top.kept = true;
                counts.kept += 1;
            }
            (Keep::Remove, false) => {
                match rustix::fs::unlinkat(&top.fd, &name, AtFlags::empty()) {
                    Ok(()) => counts.files += 1,
                    // Gone already: nothing to delete.
                    Err(Errno::NOENT) => {}
                    Err(Errno::ISDIR) => return Err(Error::Changed { path: child }),
                    Err(e) => return Err(io_err("unlink", &child, e)),
                }
            }
            (Keep::Remove | Keep::Descend, true) => {
                let fd = open_dir_checked(top.fd.as_fd(), &name, &st, &child)?;
                let names = read_names(fd.as_fd(), &child)?;
                stack.push(Frame {
                    fd,
                    path: child,
                    name,
                    names,
                    kept: false,
                    keep_self: action == Keep::Descend,
                });
            }
        }
    }
}

/// Opens the directory `name` in `dirfd` without following a symlink or
/// entering another mount, and checks it is the entry `st` described.
fn open_dir_checked(
    dirfd: BorrowedFd<'_>,
    name: &OsStr,
    st: &Stat,
    path: &Path,
) -> Result<OwnedFd> {
    let fd = match open_name(
        dirfd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        0,
        true,
        path,
    ) {
        Ok(fd) => fd,
        Err(e)
            if matches!(
                e.raw_os_error().map(Errno::from_raw_os_error),
                Some(Errno::LOOP | Errno::NOTDIR | Errno::XDEV | Errno::NOENT)
            ) =>
        {
            return Err(Error::Changed {
                path: path.to_owned(),
            });
        }
        Err(e) => return Err(e),
    };
    let now = meta::statx_at(fd.as_fd(), OsStr::new(""), path)?;
    let same = (now.ino, now.dev_major, now.dev_minor, now.mnt_id)
        == (st.ino, st.dev_major, st.dev_minor, st.mnt_id);
    if !same || now.mount_root {
        return Err(Error::Changed {
            path: path.to_owned(),
        });
    }
    Ok(fd)
}

/// The names in a directory, without `.` and `..`.
fn read_names(fd: BorrowedFd<'_>, path: &Path) -> Result<Vec<OsString>> {
    let dir = rustix::fs::Dir::read_from(fd).map_err(|e| io_err("opendir", path, e))?;
    let mut names = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|e| io_err("readdir", path, e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests;
