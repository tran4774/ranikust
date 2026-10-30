//! Copying a file or a directory's contents from one root into another
//! (`COPY` and `ADD` of local sources, `COPY --from`), and single entries
//! between roots (a hardlink into a lower layer, Q-055).
//!
//! Both sides are reached through file descriptors: the source is walked by
//! `(directory fd, name)` with every entry pinned by an `O_PATH | O_NOFOLLOW`
//! fd before its type is read, and only a pinned regular file or directory is
//! ever opened for reading. The destination is written as extraction writes
//! it: never following a last name, never crossing a mount.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType as RawFileType, Mode, OFlags};
use rustix::io::Errno;

use crate::delete;
use crate::error::{Error, Result, io_err};
use crate::meta::{self, FileType, Handle, Meta, Stat, Xattrs};
use crate::path;
use crate::root::{self, Mounts, RootPath};
use crate::user::Owner;

/// Mode of a directory until its own mode is set at the end.
const DIR_PENDING_MODE: u32 = 0o700;
/// Mode of the directories a copy creates that the source does not list:
/// the parents of the destination, and the destination of a directory's
/// contents (`kaniko@44aa7ddc0 pkg/util/fs_util.go:874`).
const CREATED_DIR_MODE: u32 = 0o755;

/// How [`copy`] sets ownership and modes; the builder maps `--chown`,
/// `--chmod` and the kind of source onto it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyOpts {
    /// Owner of every copied entry and of the directories the copy creates.
    /// `None` keeps each source entry's owner, as `COPY --from` without
    /// `--chown` does; the parents it creates then keep the process's
    /// owner. Context sources without `--chown` pass `0:0`, Docker's rule.
    pub owner: Option<Owner>,
    /// Permission bits (`& 0o7777`) for every copied entry but symlinks,
    /// directories included, as `--chmod` in octal. `None` keeps the raw
    /// bits of the source.
    pub chmod: Option<u32>,
}

/// Copies `from` in the root `src` to `to` in the root `dst`.
///
/// - `from` is resolved inside `src`, following symlinks there, the last
///   one included [open: Q-120]; `""` or `/` names the root of `src`.
/// - A directory has its contents copied into the directory `to`: `to`
///   and its missing parents are created (mode `0755`, owner
///   [`CopyOpts::owner`]; `to` gets the source directory's owner when
///   that is `None`, and its times). An existing `to` is merged into and
///   keeps its metadata; a symlink there is followed inside `dst`, a
///   non-directory replaced.
/// - Anything else is copied to exactly `to`, whose missing parents are
///   created; `to` is the file's own name, which the builder derives from
///   the destination the Dockerfile gives. An existing non-directory there
///   is replaced, never followed; an existing directory is an error.
///
/// Inside a copied directory, symlinks are copied as they are, never
/// followed; hardlinks between copied entries are kept; FIFOs and device
/// nodes are recreated, never opened (a device node needs `CAP_MKNOD`);
/// sockets are skipped. Each entry gets the source's owner or
/// [`CopyOpts::owner`], raw mode or [`CopyOpts::chmod`], modification time
/// (also as access time) and `security.capability`. Directories over
/// directories merge; any other entry in the way is replaced, except that a
/// non-directory never replaces a directory. Directory modes and times are
/// set last, deepest first.
///
/// Errors leave what was written for the caller (the snapshot) to discard.
///
/// # Errors
///
/// [`Error::Io`] from the first failing operation (a missing source,
/// `ENOTDIR` on the way to `to`, a device node or a foreign owner without
/// privileges); [`Error::InvalidPath`] when `to` climbs above the root,
/// names the root for a file, lies on or below a mount inside `dst`, or
/// inside the source directory (one root as both, as `cp` refuses), when a
/// file meets a directory, or when the source tree holds a mount below
/// `from` (source roots must not hold live mounts).
pub fn copy(
    src: &RootPath,
    from: impl AsRef<Path>,
    dst: &RootPath,
    to: impl AsRef<Path>,
    o: &CopyOpts,
) -> Result<()> {
    let span = tracing::info_span!(
        "rnk_rootfs.copy",
        files = tracing::field::Empty,
        dirs = tracing::field::Empty,
        bytes = tracing::field::Empty,
    );
    let _entered = span.enter();
    let mut c = Copier {
        dst,
        o: *o,
        dst_dirs: HashSet::new(),
        links: HashMap::new(),
        stats: Stats::default(),
    };
    let r = c.run(src, from.as_ref(), to.as_ref());
    span.record("files", c.stats.files);
    span.record("dirs", c.stats.dirs);
    span.record("bytes", c.stats.bytes);
    r
}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    files: u64,
    dirs: u64,
    bytes: u64,
}

/// An inode: device major and minor, inode number.
type Inode = (u32, u32, u64);

struct Copier<'r> {
    dst: &'r RootPath,
    o: CopyOpts,
    /// Every destination directory this copy created or entered. A source
    /// directory among them is the destination inside the source (`COPY .
    /// /workspace/sub` on a live root), which would recurse without end.
    dst_dirs: HashSet<Inode>,
    /// The first copy of each source inode with several links, so that
    /// later names link to it.
    links: HashMap<Inode, PathBuf>,
    stats: Stats,
}

/// One source directory being copied. One frame per level holds two fds,
/// so a deep tree needs a raised `RLIMIT_NOFILE`, as deletion does.
struct Frame {
    /// The source directory, `O_RDONLY | O_DIRECTORY`.
    src: OwnedFd,
    /// The destination directory, `O_PATH | O_DIRECTORY`.
    dst: OwnedFd,
    /// The destination's mode and times, set through `dst` when the frame
    /// is done, so that its entries neither move the mtime nor hit a
    /// read-only directory; `None` leaves an existing destination as it is.
    pending: Option<Meta>,
    /// Their paths inside their roots, for messages and bookkeeping.
    from: PathBuf,
    to: PathBuf,
    /// Names still to copy, last first.
    names: Vec<OsString>,
}

impl Copier<'_> {
    fn run(&mut self, src: &RootPath, from: &Path, to: &Path) -> Result<()> {
        let to = path::clean(to).ok_or_else(|| Error::InvalidPath {
            path: to.to_owned(),
            reason: "the destination climbs above the root",
        })?;
        let pinned = src.resolve(from, OFlags::PATH, Mode::empty())?;
        let st = meta::statx_at(pinned.as_fd(), OsStr::new(""), from)?;
        match st.file_type() {
            FileType::Directory => {
                let sfd = meta::reopen(pinned.as_fd(), OFlags::RDONLY | OFlags::DIRECTORY, from)?;
                let (dfd, pending) = self.top_dir(&to, &st)?;
                self.check_not_inside(&st, from)?;
                self.walk(sfd, dfd, pending, from, &to)
            }
            FileType::Socket => Ok(()),
            _ => {
                let (parent, name) = path::entry(&to)?;
                let pfd = self.parents(&parent)?;
                self.node(pinned.as_fd(), &st, None, pfd.as_fd(), name, &to)
            }
        }
    }

    fn walk(
        &mut self,
        src: OwnedFd,
        dst: OwnedFd,
        pending: Option<Meta>,
        from: &Path,
        to: &Path,
    ) -> Result<()> {
        let names = sorted_names(src.as_fd(), from)?;
        let mut stack = vec![Frame {
            src,
            dst,
            pending,
            from: from.to_owned(),
            to: to.to_owned(),
            names,
        }];
        while let Some(top) = stack.last_mut() {
            let Some(name) = top.names.pop() else {
                // Its entries are done: the directory's own mode and times,
                // through the fd it was written through, deepest first.
                if let Some(frame) = stack.pop()
                    && let Some(m) = &frame.pending
                {
                    let h = Handle::Path {
                        fd: frame.dst.as_fd(),
                        file_type: FileType::Directory,
                        parent: None,
                    };
                    meta::apply(&h, m, &frame.to)?;
                }
                continue;
            };
            let from = top.from.join(&name);
            let to = top.to.join(&name);
            // Below the source given, never into another mount: a live
            // `/proc` or `/sys` there would be read as files.
            let pin = root::open_name(
                top.src.as_fd(),
                &name,
                OFlags::PATH | OFlags::NOFOLLOW,
                0,
                true,
                &from,
            )
            .map_err(|e| on_mount(e, &from))?;
            let st = meta::statx_at(pin.as_fd(), OsStr::new(""), &from)?;
            match st.file_type() {
                FileType::Directory => {
                    self.check_not_inside(&st, &from)?;
                    let s = meta::reopen(pin.as_fd(), OFlags::RDONLY | OFlags::DIRECTORY, &from)?;
                    let (d, m) = self.dir_entry(top.dst.as_fd(), &name, &to, &st)?;
                    let names = sorted_names(s.as_fd(), &from)?;
                    stack.push(Frame {
                        src: s,
                        dst: d,
                        pending: Some(m),
                        from,
                        to,
                        names,
                    });
                }
                // A socket is bound to a running process; there is nothing
                // to copy.
                FileType::Socket => {}
                _ => {
                    let sparent = Some((top.src.as_fd(), name.as_os_str()));
                    self.node(pin.as_fd(), &st, sparent, top.dst.as_fd(), &name, &to)?;
                }
            }
        }
        Ok(())
    }

    /// The destination directory of a directory's contents, and its
    /// pending metadata: created (mode `0755`, the source's times), or
    /// existing and left as it is (`None`).
    fn top_dir(&mut self, to: &Path, st: &Stat) -> Result<(OwnedFd, Option<Meta>)> {
        if to.as_os_str().is_empty() {
            let fd = self.dst.resolve_dir_in(Path::new(""), Mounts::Stay)?;
            return self.entered(fd, None, to);
        }
        let (parent, name) = path::entry(to)?;
        let pfd = self.parents(&parent)?;
        match self.existing(pfd.as_fd(), name, to)? {
            Some(e) if e.file_type() == FileType::Directory => {
                let fd = open_dir(pfd.as_fd(), name, to)?;
                return self.entered(fd, None, to);
            }
            Some(e) if e.file_type() == FileType::Symlink => {
                let fd = self
                    .dst
                    .resolve_dir_in(to, Mounts::Stay)
                    .map_err(|e| on_mount(e, to))?;
                return self.entered(fd, None, to);
            }
            Some(_) => unlink(pfd.as_fd(), name, to)?,
            None => {}
        }
        rustix::fs::mkdirat(&pfd, name, Mode::from_raw_mode(DIR_PENDING_MODE))
            .map_err(|e| io_err("mkdir", to, e))?;
        let fd = open_dir(pfd.as_fd(), name, to)?;
        let (uid, gid) = self.owner_of(st);
        self.set_owner(fd.as_fd(), pfd.as_fd(), name, uid, gid, to)?;
        self.stats.dirs += 1;
        let pending = Meta {
            mode: Some(CREATED_DIR_MODE),
            atime: Some(st.mtime),
            mtime: Some(st.mtime),
            ..Meta::default()
        };
        self.entered(fd, Some(pending), to)
    }

    /// A directory inside the copied tree, created or merged into an
    /// existing one, and the source's mode and times for it.
    fn dir_entry(
        &mut self,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        to: &Path,
        st: &Stat,
    ) -> Result<(OwnedFd, Meta)> {
        match self.existing(pfd, name, to)? {
            Some(e) if e.file_type() == FileType::Directory => {}
            Some(_) => {
                unlink(pfd, name, to)?;
                mkdir(pfd, name, to)?;
            }
            None => mkdir(pfd, name, to)?,
        }
        let fd = open_dir(pfd, name, to)?;
        let (uid, gid) = self.owner_of(st);
        self.set_owner(fd.as_fd(), pfd, name, uid, gid, to)?;
        self.stats.dirs += 1;
        let pending = Meta {
            mode: Some(self.mode_of(st)),
            atime: Some(st.mtime),
            mtime: Some(st.mtime),
            ..Meta::default()
        };
        let (fd, _) = self.entered(fd, None, to)?;
        Ok((fd, pending))
    }

    /// Records the destination directory `fd` as written by this copy.
    fn entered(
        &mut self,
        fd: OwnedFd,
        pending: Option<Meta>,
        to: &Path,
    ) -> Result<(OwnedFd, Option<Meta>)> {
        let st = meta::statx_at(fd.as_fd(), OsStr::new(""), to)?;
        self.dst_dirs.insert((st.dev_major, st.dev_minor, st.ino));
        Ok((fd, pending))
    }

    /// Refuses the source directory `st` when this copy writes into it: the
    /// destination lies inside the source, as `cp` refuses.
    fn check_not_inside(&self, st: &Stat, from: &Path) -> Result<()> {
        if self
            .dst_dirs
            .contains(&(st.dev_major, st.dev_minor, st.ino))
        {
            return Err(Error::InvalidPath {
                path: from.to_owned(),
                reason: "the destination lies inside the source directory",
            });
        }
        Ok(())
    }

    /// Copies the non-directory `pin` (`st`) to `name` in `pfd`, or links
    /// it to an earlier copy of the same inode.
    fn node(
        &mut self,
        pin: BorrowedFd<'_>,
        st: &Stat,
        sparent: Option<(BorrowedFd<'_>, &OsStr)>,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        to: &Path,
    ) -> Result<()> {
        match self.existing(pfd, name, to)? {
            Some(e) if e.file_type() == FileType::Directory => {
                return Err(Error::InvalidPath {
                    path: to.to_owned(),
                    reason: "a directory is in the way of a file",
                });
            }
            Some(_) => unlink(pfd, name, to)?,
            None => {}
        }
        let key = (st.dev_major, st.dev_minor, st.ino);
        if st.nlink > 1
            && let Some(first) = self.links.get(&key)
        {
            let (fparent, fname) = path::entry(first)?;
            let fpfd = self
                .dst
                .resolve_dir_in(&fparent, Mounts::Stay)
                .map_err(|e| on_mount(e, to))?;
            rustix::fs::linkat(&fpfd, fname, pfd, name, AtFlags::empty())
                .map_err(|e| io_err("link", to, e))?;
            self.stats.files += 1;
            return Ok(());
        }
        let (uid, gid) = self.owner_of(st);
        let m = Meta {
            uid: Some(uid),
            gid: Some(gid),
            mode: Some(self.mode_of(st)),
            atime: Some(st.mtime),
            mtime: Some(st.mtime),
            xattrs: Vec::new(),
        };
        self.stats.bytes += copy_node(pin, st, sparent, Xattrs::Capability, pfd, name, m, to)?;
        self.stats.files += 1;
        if st.nlink > 1 {
            self.links.insert(key, to.to_owned());
        }
        Ok(())
    }

    /// Creates the missing directories of `parent` (cleaned, relative),
    /// following existing symlinks inside the root; returns its `O_PATH` fd.
    fn parents(&mut self, parent: &Path) -> Result<OwnedFd> {
        let mut cur = self.dst.resolve_dir_in(Path::new(""), Mounts::Stay)?;
        let mut prefix = PathBuf::new();
        for c in parent.components() {
            let Component::Normal(name) = c else {
                continue;
            };
            prefix.push(name);
            match self.dst.resolve_dir_in(&prefix, Mounts::Stay) {
                Ok(fd) => cur = fd,
                Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                    let created = match rustix::fs::mkdirat(
                        &cur,
                        name,
                        Mode::from_raw_mode(CREATED_DIR_MODE),
                    ) {
                        Ok(()) => true,
                        // A dangling symlink: resolving it fails below.
                        Err(Errno::EXIST) => false,
                        Err(e) => return Err(io_err("mkdir", &prefix, e)),
                    };
                    let fd = self
                        .dst
                        .resolve_dir_in(&prefix, Mounts::Stay)
                        .map_err(|e| on_mount(e, &prefix))?;
                    if created {
                        let m = Meta {
                            uid: self.o.owner.map(|o| o.uid),
                            gid: self.o.owner.map(|o| o.gid),
                            mode: Some(CREATED_DIR_MODE),
                            ..Meta::default()
                        };
                        let h = Handle::Path {
                            fd: fd.as_fd(),
                            file_type: FileType::Directory,
                            parent: Some((cur.as_fd(), name)),
                        };
                        meta::apply(&h, &m, &prefix)?;
                        self.stats.dirs += 1;
                    }
                    cur = fd;
                }
                Err(e) => return Err(on_mount(e, &prefix)),
            }
        }
        Ok(cur)
    }

    /// The entry `name` in `pfd`, if any; one on another mount, or a mount
    /// point, is refused.
    fn existing(&self, pfd: BorrowedFd<'_>, name: &OsStr, to: &Path) -> Result<Option<Stat>> {
        match meta::statx_at(pfd, name, to) {
            Ok(st) if st.mnt_id != self.dst.mnt_id() || st.mount_root => Err(crosses_mount(to)),
            Ok(st) => Ok(Some(st)),
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_owner(
        &self,
        fd: BorrowedFd<'_>,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        uid: u32,
        gid: u32,
        to: &Path,
    ) -> Result<()> {
        let h = Handle::Path {
            fd,
            file_type: FileType::Directory,
            parent: Some((pfd, name)),
        };
        let m = Meta {
            uid: Some(uid),
            gid: Some(gid),
            ..Meta::default()
        };
        meta::apply(&h, &m, to)
    }

    fn owner_of(&self, st: &Stat) -> (u32, u32) {
        self.o.owner.map_or((st.uid, st.gid), |o| (o.uid, o.gid))
    }

    fn mode_of(&self, st: &Stat) -> u32 {
        self.o
            .chmod
            .map_or(st.permissions(), |m| m & meta::PERMISSION_BITS)
    }
}

/// Copies the non-directory pinned by `pin` (an `O_PATH | O_NOFOLLOW` fd;
/// `st` its `statx`; `sparent` its pinned parent and name, needed for a
/// symlink's xattrs) to the free name `name` in the destination directory
/// `pfd`, with the metadata `m` plus the source's xattrs as `which` says.
/// A regular file is read through its pinned inode; a FIFO or a device
/// node is recreated with `mknodat`, never opened. Returns the bytes
/// written. `path` is the destination, for messages.
#[expect(
    clippy::too_many_arguments,
    reason = "a source and a destination, each as fd, name and stat"
)]
pub(crate) fn copy_node(
    pin: BorrowedFd<'_>,
    st: &Stat,
    sparent: Option<(BorrowedFd<'_>, &OsStr)>,
    which: Xattrs,
    pfd: BorrowedFd<'_>,
    name: &OsStr,
    mut m: Meta,
    path: &Path,
) -> Result<u64> {
    let file_type = st.file_type();
    m.xattrs = meta::read_xattrs(pin, file_type, sparent, which, path)?;
    match file_type {
        FileType::Regular => {
            let src = meta::reopen(pin, OFlags::RDONLY, path)?;
            let out = root::open_name(
                pfd,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                0o600,
                true,
                path,
            )?;
            let mut out = std::fs::File::from(out);
            // `copy_file_range` on Linux: a reflink where the filesystem
            // shares extents.
            let n = std::io::copy(&mut std::fs::File::from(src), &mut out).map_err(|source| {
                Error::Io {
                    op: "copy",
                    path: path.to_owned(),
                    source,
                }
            })?;
            meta::apply(&Handle::Open(out.as_fd()), &m, path)?;
            Ok(n)
        }
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(pin, "", Vec::new())
                .map_err(|e| io_err("readlink", path, e))?;
            rustix::fs::symlinkat(target.as_c_str(), pfd, name)
                .map_err(|e| io_err("symlink", path, e))?;
            m.mode = None;
            root::set_meta_at(pfd, name, &m, None, path).map(|_| 0)
        }
        FileType::Fifo | FileType::CharDevice | FileType::BlockDevice => {
            let t = match file_type {
                FileType::Fifo => RawFileType::Fifo,
                FileType::CharDevice => RawFileType::CharacterDevice,
                _ => RawFileType::BlockDevice,
            };
            rustix::fs::mknodat(
                pfd,
                name,
                t,
                Mode::from_raw_mode(0o600),
                rustix::fs::makedev(st.rdev_major, st.rdev_minor),
            )
            .map_err(|e| io_err("mknod", path, e))?;
            root::set_meta_at(pfd, name, &m, None, path).map(|_| 0)
        }
        FileType::Directory | FileType::Socket | FileType::Unknown => Err(Error::InvalidPath {
            path: path.to_owned(),
            reason: "only a file, symlink, FIFO or device node is copied as one entry",
        }),
    }
}

/// The names in `fd`, sorted so that the copy, and which name of a
/// hardlinked inode is copied first, never depend on directory order.
fn sorted_names(fd: BorrowedFd<'_>, path: &Path) -> Result<Vec<OsString>> {
    let mut names = delete::read_names(fd, path)?;
    names.sort_unstable_by(|a, b| b.cmp(a));
    Ok(names)
}

fn mkdir(pfd: BorrowedFd<'_>, name: &OsStr, to: &Path) -> Result<()> {
    rustix::fs::mkdirat(pfd, name, Mode::from_raw_mode(DIR_PENDING_MODE))
        .map_err(|e| io_err("mkdir", to, e))
}

fn unlink(pfd: BorrowedFd<'_>, name: &OsStr, to: &Path) -> Result<()> {
    rustix::fs::unlinkat(pfd, name, AtFlags::empty()).map_err(|e| io_err("unlink", to, e))
}

/// The directory `name` in `pfd`, as an `O_PATH` fd: no symlink followed, no
/// mount entered.
fn open_dir(pfd: BorrowedFd<'_>, name: &OsStr, to: &Path) -> Result<OwnedFd> {
    root::open_name(
        pfd,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        0,
        true,
        to,
    )
    .map_err(|e| on_mount(e, to))
}

fn crosses_mount(to: &Path) -> Error {
    Error::InvalidPath {
        path: to.to_owned(),
        reason: "lies on or below a mount point inside the root; a copy never crosses a mount",
    }
}

/// Maps `EXDEV` from a [`Mounts::Stay`] resolution to [`crosses_mount`].
fn on_mount(e: Error, to: &Path) -> Error {
    if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) {
        crosses_mount(to)
    } else {
        e
    }
}

#[cfg(test)]
mod tests;
