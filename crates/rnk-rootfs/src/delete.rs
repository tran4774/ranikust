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
/// unlinked, never followed. Depth is limited neither by `PATH_MAX` nor by
/// `RLIMIT_NOFILE`: at most 32 directory descriptors are held, the nearest
/// ancestors of the directory being emptied; a farther one is closed and
/// reopened through `..` on the way back up, checked to be the directory
/// it was (inode, device and mount id), else [`Error::Changed`].
/// Memory is linear in the entries on the walk's stack (each level keeps
/// its name, its `statx` and the names of its directory not yet visited)
/// plus one path buffer, never quadratic in depth.
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
/// Overlay's own xattrs ([`crate::is_overlay_own`]) go from every entry
/// kept and from the root, as a copy leaves them out. A copy of the root
/// that leaves out what `remove` names ([`crate::copy_filtered`] with the
/// same filter, which also leaves out `CopyOpts::skip_special`'s nodes when
/// `remove` names them) then gives the same entries, contents, owners,
/// modes, modification times, xattrs, links and hardlinks between kept
/// names. What a `RUN` can still tell apart, which no layer holds: access
/// and change times, directory sizes (ext4 and xfs keep them after
/// deletions), the order `readdir` lists a directory in, and inode flags a
/// copy does not carry (`chattr` flags a non-privileged owner may set). Kaniko mode hands a stage's directory to
/// the stage built on it this way instead of copying it (ADR-0061).
///
/// Walks and deletes as [`delete_contents`] does: by `(parent fd, name)`,
/// never following a symlink, each directory checked against the `statx`
/// taken before opening it, and reopened through `..` checked against it
/// when the walk climbs back past the descriptors it holds (64 at most:
/// its own walk and, inside it, the deletion of a directory it removes). A
/// mount anywhere below the root (another mount id, or a mount root) is an
/// error, as a copy refuses one: nothing would be deleted across it, so it
/// would stay where the copy would not have it.
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
    /// A non-directory `prune` keeps has a hardlink outside the root: its
    /// link count when first seen was more than the names `prune` found for
    /// it in the root, kept or deleted, whatever order it walked them in.
    pub linked_outside: bool,
}

/// Per inode with several links when first seen: that link count, the
/// names seen for it since in the root, and whether `prune` keeps one.
///
/// Exact: every name is seen before it is deleted (`prune`'s own entries
/// and those `remove_below` deletes inside a directory removed whole), so
/// at an inode's first sight every other name of it is still there, inside
/// the root or outside; deleting names only lowers the count later. An
/// inode whose names in the root are all deleted leaves nothing behind.
#[derive(Default)]
struct Links(std::collections::HashMap<(u32, u32, u64), LinkCount>);

#[derive(Clone, Copy)]
struct LinkCount {
    nlink: u32,
    seen: u32,
    kept: bool,
}

impl Links {
    fn key(st: &Stat) -> (u32, u32, u64) {
        (st.dev_major, st.dev_minor, st.ino)
    }

    fn saw(&mut self, st: &Stat) {
        if st.file_type() == FileType::Directory {
            return;
        }
        if let Some(e) = self.0.get_mut(&Self::key(st)) {
            e.seen += 1;
        } else if st.nlink > 1 {
            self.0.insert(
                Self::key(st),
                LinkCount {
                    nlink: st.nlink,
                    seen: 1,
                    kept: false,
                },
            );
        }
    }

    /// `st`, seen already, is a name `prune` keeps.
    fn keep(&mut self, st: &Stat) {
        if let Some(e) = self.0.get_mut(&Self::key(st)) {
            e.kept = true;
        }
    }

    fn outside(&self) -> bool {
        self.0.values().any(|e| e.kept && e.nlink > e.seen)
    }
}

/// What [`prune`] keeps for one directory it walks. Its path is the walk's
/// one path buffer, not the frame's: a frame per level holding its full
/// path would take memory quadratic in depth.
struct PruneFrame {
    names: Vec<OsString>,
    /// Something in it was deleted: its times (the [`Dir`]'s `statx`,
    /// taken before anything in it was deleted) are set back when it is
    /// done.
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
    // Overlay's own xattrs go from every entry kept, the root included: a
    // copy leaves them out (`meta::Xattrs::Copy`).
    meta::strip_overlay_own(start.as_fd(), FileType::Directory, None, Path::new(""))?;
    let names = read_names(start.as_fd(), Path::new(""))?;
    let mut stack = Dirs::new(Dir {
        fd: start,
        st,
        data: PruneFrame {
            names,
            changed: false,
        },
    });
    // The path of the directory on top of the stack, then, while an entry
    // of it is handled, of that entry.
    let mut cur = PathBuf::new();
    while let Some(top) = stack.top_mut() {
        let Some(name) = top.data.names.pop() else {
            let parent = cur.parent().unwrap_or(Path::new(""));
            if let Some(done) = stack.pop(parent)?
                && done.data.changed
            {
                let times = rustix::fs::Timestamps {
                    last_access: done.st.atime.to_timespec(),
                    last_modification: done.st.mtime.to_timespec(),
                };
                rustix::fs::futimens(&done.fd, &times).map_err(|e| io_err("utimensat", &cur, e))?;
            }
            cur.pop();
            continue;
        };
        cur.push(&name);
        // Nothing else writes the root while it is pruned: an entry that
        // goes meanwhile is a change, not something to skip.
        let st = match meta::statx_at(top.fd.as_fd(), &name, &cur) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                return Err(Error::Changed { path: cur });
            }
            Err(e) => return Err(e),
        };
        if root.off_mount(&st) {
            return Err(Error::InvalidPath {
                path: cur,
                reason: "a mount below the root",
            });
        }
        links.saw(&st);
        let file_type = st.file_type();
        let is_dir = file_type == FileType::Directory;
        if !remove(&cur, file_type) {
            if is_dir {
                let fd = open_dir_checked(top.fd.as_fd(), &name, &st, &cur)?;
                meta::strip_overlay_own(fd.as_fd(), file_type, None, &cur)?;
                let names = read_names(fd.as_fd(), &cur)?;
                // `cur` stays this directory's path while it is walked.
                stack.push(Dir {
                    fd,
                    st,
                    data: PruneFrame {
                        names,
                        changed: false,
                    },
                });
                continue;
            }
            let pin = pin_checked(top.fd.as_fd(), &name, &st, &cur)?;
            meta::strip_overlay_own(pin.as_fd(), file_type, Some((top.fd.as_fd(), &name)), &cur)?;
            links.keep(&st);
            counts.kept += 1;
            cur.pop();
            continue;
        }
        top.data.changed = true;
        if is_dir {
            let fd = open_dir_checked(top.fd.as_fd(), &name, &st, &cur)?;
            let mut all = |_: &Path, _: FileType| Keep::Remove;
            let mut seen = |st: &Stat| links.saw(st);
            // Entries on another mount are kept by `remove_below`, which
            // then cannot remove the directory: refused below as a change.
            remove_below_seen(root, fd, &cur, &mut all, counts, &mut seen)?;
            match rustix::fs::unlinkat(&top.fd, &name, AtFlags::REMOVEDIR) {
                Ok(()) => counts.dirs += 1,
                Err(Errno::NOTDIR | Errno::NOTEMPTY | Errno::NOENT) => {
                    return Err(Error::Changed { path: cur });
                }
                Err(e) => return Err(io_err("rmdir", &cur, e)),
            }
        } else {
            match rustix::fs::unlinkat(&top.fd, &name, AtFlags::empty()) {
                Ok(()) => counts.files += 1,
                Err(Errno::NOENT | Errno::ISDIR) => return Err(Error::Changed { path: cur }),
                Err(e) => return Err(io_err("unlink", &cur, e)),
            }
        }
        cur.pop();
    }
    Ok(())
}

/// Most descriptors one deletion walk holds at a time, whatever the depth
/// of the tree: the directory it works in, its nearest ancestors, and the
/// two a step opens briefly (the next directory, and the duplicate it is
/// listed through). A farther ancestor's descriptor is closed and the
/// directory reopened through `..` when the walk climbs back to it, checked
/// to be the one it was (as GNU `fts` does for `rm -rf`), so a tree deeper
/// than `RLIMIT_NOFILE` is deleted like any other (`tests/descriptors.rs`
/// leaves the walk exactly this many).
pub(crate) const OPEN_DIRS: usize = 32;

/// A directory a deletion walk works in: its descriptor, its identity and
/// what the walk keeps for it.
struct Dir<T> {
    /// `O_RDONLY | O_DIRECTORY` fd.
    fd: OwnedFd,
    /// Its `statx` when it was opened, which a reopened descriptor must
    /// match.
    st: Stat,
    data: T,
}

/// A deletion walk's stack of directories: the one it works in, on top,
/// and its ancestors in the walk. Only the `OPEN_DIRS - 2` nearest the top
/// keep a descriptor; a farther one is reopened through `..` of its child
/// when it becomes the top again, and must then be the inode, on the
/// device and mount, that it was: else [`Error::Changed`], with nothing
/// done in what `..` reached. The child is still open when its parent is
/// reopened, and `..` never leaves its mount (`RESOLVE_NO_XDEV`).
struct Dirs<T> {
    top: Option<Dir<T>>,
    /// The ancestors of `top`, nearest last; the last `open` of them hold
    /// their descriptor, the others `None`.
    below: Vec<(Option<OwnedFd>, Stat, T)>,
    open: usize,
}

impl<T> Dirs<T> {
    fn new(start: Dir<T>) -> Self {
        Self {
            top: Some(start),
            below: Vec::new(),
            open: 0,
        }
    }

    fn top_mut(&mut self) -> Option<&mut Dir<T>> {
        self.top.as_mut()
    }

    /// Makes `dir`, opened in the top directory, the top; closes the
    /// farthest ancestor still open when the stack would hold more than
    /// `OPEN_DIRS - 2` descriptors.
    fn push(&mut self, dir: Dir<T>) {
        if let Some(Dir { fd, st, data }) = self.top.replace(dir) {
            self.below.push((Some(fd), st, data));
            self.open += 1;
        }
        // Room for the two the next step opens before it pushes: the
        // directory, then the duplicate `read_names` lists it through.
        if 1 + self.open + 2 > OPEN_DIRS {
            let farthest = self.below.len() - self.open;
            if let Some(closed) = self.below.get_mut(farthest) {
                closed.0 = None;
                self.open -= 1;
            }
        }
    }

    /// Takes the top off and returns it, its descriptor still open. Its
    /// parent in the walk becomes the top, reopened through `..` from that
    /// descriptor if it was closed, and checked; `parent` is the parent's
    /// path, for errors.
    fn pop(&mut self, parent: &Path) -> Result<Option<Dir<T>>> {
        let Some(done) = self.top.take() else {
            return Ok(None);
        };
        if let Some((fd, st, data)) = self.below.pop() {
            let fd = match fd {
                Some(fd) => {
                    self.open -= 1;
                    fd
                }
                None => reopen_parent(done.fd.as_fd(), &st, parent)?,
            };
            self.top = Some(Dir { fd, st, data });
        }
        Ok(Some(done))
    }
}

/// Opens the parent of the directory `child` through `..` and checks it is
/// the directory `st` described: the same inode on the same device and
/// mount. Anything else (an ancestor renamed, moved or replaced since its
/// descriptor was closed) is [`Error::Changed`].
fn reopen_parent(child: BorrowedFd<'_>, st: &Stat, path: &Path) -> Result<OwnedFd> {
    let fd = match crate::root::open_parent(child, path) {
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
    if !now.same_entry(st) {
        return Err(Error::Changed {
            path: path.to_owned(),
        });
    }
    Ok(fd)
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

/// What a deletion walk keeps for one directory being emptied. Its path
/// is the walk's one path buffer, not the frame's: a frame per level
/// holding its full path would take memory quadratic in depth, and a `RUN`
/// can make a tree as deep as it likes.
struct Frame {
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
/// At most [`OPEN_DIRS`] directory descriptors are held, `fd` included.
pub(crate) fn remove_below(
    root: &RootPath,
    fd: OwnedFd,
    dir: &Path,
    decide: &mut dyn FnMut(&Path, FileType) -> Keep,
    counts: &mut Counts,
) -> Result<bool> {
    remove_below_seen(root, fd, dir, decide, counts, &mut |_| {})
}

/// [`remove_below`], showing `seen` the `statx` of every entry below `dir`
/// before anything is done with it ([`prune`] counts the names of each
/// inode it deletes).
fn remove_below_seen(
    root: &RootPath,
    fd: OwnedFd,
    dir: &Path,
    decide: &mut dyn FnMut(&Path, FileType) -> Keep,
    counts: &mut Counts,
    seen: &mut dyn FnMut(&Stat),
) -> Result<bool> {
    let st = meta::statx_at(fd.as_fd(), OsStr::new(""), dir)?;
    if st.mnt_id != root.mnt_id() {
        counts.kept += 1;
        return Ok(false);
    }
    let names = read_names(fd.as_fd(), dir)?;
    let mut stack = Dirs::new(Dir {
        fd,
        st,
        data: Frame {
            name: OsString::new(),
            names,
            kept: false,
            keep_self: true,
        },
    });
    // The path of the directory on top of the stack, then, while an entry
    // of it is handled, of that entry.
    let mut cur = dir.to_owned();

    loop {
        let Some(top) = stack.top_mut() else {
            // Only reached if the start frame was popped, which returns.
            return Ok(true);
        };
        let Some(name) = top.data.names.pop() else {
            let parent_path = cur.parent().unwrap_or(Path::new(""));
            let Some(done) = stack.pop(parent_path)? else {
                return Ok(true);
            };
            let Some(parent) = stack.top_mut() else {
                return Ok(!done.data.kept);
            };
            if done.data.kept || done.data.keep_self {
                parent.data.kept = true;
            } else {
                match rustix::fs::unlinkat(&parent.fd, &done.data.name, AtFlags::REMOVEDIR) {
                    Ok(()) => counts.dirs += 1,
                    // The name now holds something else than the directory
                    // just emptied (a swap), something was added to it, or
                    // it was removed meanwhile.
                    Err(Errno::NOTDIR | Errno::NOTEMPTY | Errno::NOENT) => {
                        return Err(Error::Changed { path: cur });
                    }
                    Err(e) => return Err(io_err("rmdir", &cur, e)),
                }
            }
            cur.pop();
            continue;
        };
        cur.push(&name);
        let st = match meta::statx_at(top.fd.as_fd(), &name, &cur) {
            Ok(st) => st,
            // Gone already: nothing to delete.
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                cur.pop();
                continue;
            }
            Err(e) => return Err(e),
        };
        seen(&st);
        if root.off_mount(&st) {
            top.data.kept = true;
            counts.kept += 1;
            cur.pop();
            continue;
        }
        let file_type = st.file_type();
        let action = decide(&cur, file_type);
        match (action, file_type == FileType::Directory) {
            (Keep::Whole, _) | (Keep::Descend, false) => {
                top.data.kept = true;
                counts.kept += 1;
            }
            (Keep::Remove, false) => {
                match rustix::fs::unlinkat(&top.fd, &name, AtFlags::empty()) {
                    Ok(()) => counts.files += 1,
                    // Gone already: nothing to delete.
                    Err(Errno::NOENT) => {}
                    Err(Errno::ISDIR) => return Err(Error::Changed { path: cur }),
                    Err(e) => return Err(io_err("unlink", &cur, e)),
                }
            }
            (Keep::Remove | Keep::Descend, true) => {
                let fd = open_dir_checked(top.fd.as_fd(), &name, &st, &cur)?;
                let names = read_names(fd.as_fd(), &cur)?;
                // `cur` stays this directory's path while it is emptied.
                stack.push(Dir {
                    fd,
                    st,
                    data: Frame {
                        name,
                        names,
                        kept: false,
                        keep_self: action == Keep::Descend,
                    },
                });
                continue;
            }
        }
        cur.pop();
    }
}

/// Pins the entry `name` in `dirfd` (`O_PATH | O_NOFOLLOW`, never entering
/// another mount) and checks it is the entry `st` described.
fn pin_checked(dirfd: BorrowedFd<'_>, name: &OsStr, st: &Stat, path: &Path) -> Result<OwnedFd> {
    let fd = match open_name(dirfd, name, OFlags::PATH | OFlags::NOFOLLOW, 0, true, path) {
        Ok(fd) => fd,
        Err(e)
            if matches!(
                e.raw_os_error().map(Errno::from_raw_os_error),
                Some(Errno::XDEV | Errno::NOENT)
            ) =>
        {
            return Err(Error::Changed {
                path: path.to_owned(),
            });
        }
        Err(e) => return Err(e),
    };
    let now = meta::statx_at(fd.as_fd(), OsStr::new(""), path)?;
    if !now.same_entry(st) {
        return Err(Error::Changed {
            path: path.to_owned(),
        });
    }
    Ok(fd)
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
    if !now.same_entry(st) || now.mount_root {
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
