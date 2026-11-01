//! Deleting trees inside a root through file descriptors: never following a
//! symlink, never crossing a mount (compared by mount id, never `st_dev`),
//! stopping when an entry changes under it.
//!
//! This is the mechanism only; `rnk-snapshot` and `rnk-store` decide what to
//! delete and when.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};
use crate::ignore::IgnoreList;
use crate::meta::{self, FileType, Stat};
use crate::root::{Mounts, RootPath, open_name};

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
        "rnk_rootfs.delete",
        entries = tracing::field::Empty,
        files = tracing::field::Empty,
        dirs = tracing::field::Empty,
        kept = tracing::field::Empty,
    );
    let _entered = span.enter();
    let mut counts = Counts::default();
    let mut decide = |p: &Path, _: FileType| {
        if keep.covers(p) {
            Keep::Whole
        } else {
            Keep::Remove
        }
    };
    let r = open_dir(root, Path::new(""))
        .and_then(|start| remove_below(root, start, Path::new(""), &mut decide, &mut counts));
    counts.record(&span);
    r.map(drop)
}

/// Deletes the entries of `root` that `remove` names, each with everything
/// below it, and leaves the rest exactly as it is: every directory it
/// deletes from gets its access and modification times back. `remove` gets
/// each entry's cleaned path inside the root and its type; the root itself
/// is never offered.
///
/// A copy of the root that leaves out what `remove` names
/// ([`crate::copy_filtered`] with the same filter, which also leaves out
/// `CopyOpts::skip_special`'s nodes when `remove` names them) gives the same
/// entries, contents, owners, modes, modification times, links and
/// hardlinks between kept names. Kaniko mode hands a stage's directory to
/// the stage built on it this way instead of copying it (ADR-0061).
///
/// Walks and deletes as [`delete_contents`] does: by `(parent fd, name)`,
/// never following a symlink, each directory checked against the `statx`
/// taken before opening it. A mount anywhere below the root (another mount
/// id, or a mount root) is an error, as a copy refuses one: nothing would
/// be deleted across it, so it would stay where the copy would not have it.
///
/// It also reports ([`Pruned::linked_outside`]) whether an entry it keeps
/// has more hardlinks than the names it saw for it in the root: a root
/// handed over keeps its inodes, so such a link would reach outside it.
///
/// # Errors
///
/// [`Error::InvalidPath`] for this process's own `/` (nothing is walked)
/// or a mount below the root; [`Error::Changed`] when an entry changes or
/// goes while it is walked; [`Error::Io`] on the first failing syscall.
/// What was deleted until then stays deleted.
pub fn prune(root: &RootPath, remove: &dyn Fn(&Path, FileType) -> bool) -> Result<Pruned> {
    let span = tracing::info_span!(
        "rnk_rootfs.prune",
        entries = tracing::field::Empty,
        files = tracing::field::Empty,
        dirs = tracing::field::Empty,
        kept = tracing::field::Empty,
    );
    let _entered = span.enter();
    crate::root::refuse_own_root(root, Path::new(""))?;
    let mut counts = Counts::default();
    let mut links = Links::default();
    let r = prune_walk(root, remove, &mut counts, &mut links);
    counts.record(&span);
    r.map(|()| Pruned {
        linked_outside: links.outside(),
    })
}

/// What [`prune`] found besides what it deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// A non-directory had more hardlinks than names inside the root when
    /// first seen: a link from outside the root, or (counted too, as it
    /// may be) one in a directory `prune` deleted whole.
    pub linked_outside: bool,
}

/// Per inode with several links: its link count when first seen, and the
/// names seen for it.
#[derive(Default)]
struct Links(std::collections::HashMap<(u32, u32, u64), (u32, u32)>);

impl Links {
    fn saw(&mut self, st: &Stat) {
        if st.file_type() != FileType::Directory && st.nlink > 1 {
            let e = self
                .0
                .entry((st.dev_major, st.dev_minor, st.ino))
                .or_insert((st.nlink, 0));
            e.1 += 1;
        }
    }

    /// Deleting names only lowers a link count, so the first count seen is
    /// the largest.
    fn outside(&self) -> bool {
        self.0.values().any(|&(nlink, seen)| nlink > seen)
    }
}

/// One directory [`prune`] walks.
struct PruneFrame {
    /// `O_RDONLY | O_DIRECTORY` fd: `futimens` needs an open file.
    fd: OwnedFd,
    path: PathBuf,
    /// Its `statx` before anything in it was deleted.
    st: Stat,
    names: Vec<OsString>,
    /// Something in it was deleted: its times are set back when it is done.
    changed: bool,
}

fn prune_walk(
    root: &RootPath,
    remove: &dyn Fn(&Path, FileType) -> bool,
    counts: &mut Counts,
    links: &mut Links,
) -> Result<()> {
    let start = open_dir(root, Path::new(""))?;
    let st = meta::statx_at(start.as_fd(), OsStr::new(""), Path::new(""))?;
    if st.mnt_id != root.mnt_id() {
        return Err(Error::Changed {
            path: PathBuf::new(),
        });
    }
    let names = read_names(start.as_fd(), Path::new(""))?;
    let mut stack = vec![PruneFrame {
        fd: start,
        path: PathBuf::new(),
        st,
        names,
        changed: false,
    }];
    while let Some(top) = stack.last_mut() {
        let Some(name) = top.names.pop() else {
            if let Some(done) = stack.pop()
                && done.changed
            {
                let times = rustix::fs::Timestamps {
                    last_access: done.st.atime.to_timespec(),
                    last_modification: done.st.mtime.to_timespec(),
                };
                rustix::fs::futimens(&done.fd, &times)
                    .map_err(|e| io_err("utimensat", &done.path, e))?;
            }
            continue;
        };
        let child = top.path.join(&name);
        // Nothing else writes the root while it is pruned: an entry that
        // goes meanwhile is a change, not something to skip.
        let st = match meta::statx_at(top.fd.as_fd(), &name, &child) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                return Err(Error::Changed { path: child });
            }
            Err(e) => return Err(e),
        };
        if st.mnt_id != root.mnt_id() || st.mount_root {
            return Err(Error::InvalidPath {
                path: child,
                reason: "a mount below the root",
            });
        }
        links.saw(&st);
        let file_type = st.file_type();
        let is_dir = file_type == FileType::Directory;
        if !remove(&child, file_type) {
            if is_dir {
                let fd = open_dir_checked(top.fd.as_fd(), &name, &st, &child)?;
                let names = read_names(fd.as_fd(), &child)?;
                stack.push(PruneFrame {
                    fd,
                    path: child,
                    st,
                    names,
                    changed: false,
                });
            } else {
                counts.kept += 1;
            }
            continue;
        }
        top.changed = true;
        if is_dir {
            let fd = open_dir_checked(top.fd.as_fd(), &name, &st, &child)?;
            let mut all = |_: &Path, _: FileType| Keep::Remove;
            // Entries on another mount are kept by `remove_below`, which
            // then cannot remove the directory: refused below as a change.
            remove_below(root, fd, &child, &mut all, counts)?;
            match rustix::fs::unlinkat(&top.fd, &name, AtFlags::REMOVEDIR) {
                Ok(()) => counts.dirs += 1,
                Err(Errno::NOTDIR | Errno::NOTEMPTY | Errno::NOENT) => {
                    return Err(Error::Changed { path: child });
                }
                Err(e) => return Err(io_err("rmdir", &child, e)),
            }
        } else {
            match rustix::fs::unlinkat(&top.fd, &name, AtFlags::empty()) {
                Ok(()) => counts.files += 1,
                Err(Errno::NOENT | Errno::ISDIR) => return Err(Error::Changed { path: child }),
                Err(e) => return Err(io_err("unlink", &child, e)),
            }
        }
    }
    Ok(())
}

/// Opens the directory `dir` inside `root` for a deletion walk: no final
/// symlink followed, no mount entered on the way (`EXDEV`).
pub(crate) fn open_dir(root: &RootPath, dir: &Path) -> Result<OwnedFd> {
    root.resolve_in(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
        Mounts::Stay,
    )
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
        span.record("entries", self.files + self.dirs);
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

/// Deletes the entries below the directory `fd` (an `O_RDONLY` directory fd,
/// from [`open_dir`] or [`open_dir_checked`]; `dir` is its cleaned path
/// inside `root`, `""` for the root) as `decide` says; the directory itself
/// stays. Returns whether nothing below it was kept.
///
/// `decide` gets each entry's cleaned path and type; entries on another
/// mount, and mount roots, are kept without asking. Starting from an
/// opened fd means the directory checked by the caller is the one emptied.
pub(crate) fn remove_below(
    root: &RootPath,
    fd: OwnedFd,
    dir: &Path,
    decide: &mut dyn FnMut(&Path, FileType) -> Keep,
    counts: &mut Counts,
) -> Result<bool> {
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
                    // just emptied (a swap), something was added to it, or
                    // it was removed meanwhile.
                    Err(Errno::NOTDIR | Errno::NOTEMPTY | Errno::NOENT) => {
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
pub(crate) fn open_dir_checked(
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

/// The names in a directory, without `.` and `..`. A directory removed
/// since it was opened (`ENOENT` on reopening `.`) is a change.
pub(crate) fn read_names(fd: BorrowedFd<'_>, path: &Path) -> Result<Vec<OsString>> {
    let dir = rustix::fs::Dir::read_from(fd).map_err(|e| match e {
        Errno::NOENT => Error::Changed {
            path: path.to_owned(),
        },
        e => io_err("opendir", path, e),
    })?;
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
