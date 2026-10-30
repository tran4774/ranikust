//! `RootPath`: every path inside a root, resolved by the kernel from a root fd.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};
use crate::meta::{self, FileType, Handle, Meta, Stat, Xattrs};
use crate::path::{self, Target};

/// How every path inside a root is resolved: `..` and absolute symlinks are
/// clamped to the root, and magic links (`/proc/<pid>/root`, `/proc/self/fd/N`
/// and the like) are refused.
const IN_ROOT: ResolveFlags = ResolveFlags::IN_ROOT.union(ResolveFlags::NO_MAGICLINKS);

/// Whether a resolution may enter another mount below the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mounts {
    /// Mounts inside the root are part of it (a `RUN` bind mount, `/proc`
    /// in a container root).
    Cross,
    /// Stay on the root's own mount: extraction and deletion never write
    /// across a mount point.
    Stay,
}

/// How a single name is opened inside an already resolved directory: no
/// traversal at all.
const ONE_NAME: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

/// How [`RootPath::open_at`] opens a path. Always `O_CLOEXEC`; `O_NOCTTY`
/// unless only a path handle is asked for.
#[derive(Clone, Copy, Debug)]
pub struct OpenHow {
    flags: OFlags,
    mode: u32,
}

impl OpenHow {
    /// Open for reading.
    #[must_use]
    pub const fn read() -> Self {
        Self {
            flags: OFlags::RDONLY,
            mode: 0,
        }
    }

    /// Open for writing.
    #[must_use]
    pub const fn write() -> Self {
        Self {
            flags: OFlags::WRONLY,
            mode: 0,
        }
    }

    /// Open for reading and writing.
    #[must_use]
    pub const fn read_write() -> Self {
        Self {
            flags: OFlags::RDWR,
            mode: 0,
        }
    }

    /// A path handle only (`O_PATH`): no I/O, no permission needed on the
    /// entry itself.
    #[must_use]
    pub const fn path() -> Self {
        Self {
            flags: OFlags::PATH,
            mode: 0,
        }
    }

    /// Fail unless the path is a directory (`O_DIRECTORY`).
    #[must_use]
    pub const fn directory(self) -> Self {
        self.with(OFlags::DIRECTORY)
    }

    /// Do not follow a symlink in the last component (`O_NOFOLLOW`): open
    /// fails with `ELOOP`, or, with [`OpenHow::path`], returns the symlink
    /// itself. Symlinks earlier in the path are followed, inside the root.
    #[must_use]
    pub const fn no_follow(self) -> Self {
        self.with(OFlags::NOFOLLOW)
    }

    /// Create the file if missing (`O_CREAT`), with `mode & !umask`.
    #[must_use]
    pub const fn create(self, mode: u32) -> Self {
        Self {
            flags: self.flags.union(OFlags::CREATE),
            mode,
        }
    }

    /// Create the file, failing if anything exists under the name, a
    /// symlink included (`O_CREAT | O_EXCL`), with `mode & !umask`.
    #[must_use]
    pub const fn create_new(self, mode: u32) -> Self {
        Self {
            flags: self.flags.union(OFlags::CREATE).union(OFlags::EXCL),
            mode,
        }
    }

    /// Truncate to zero length (`O_TRUNC`).
    #[must_use]
    pub const fn truncate(self) -> Self {
        self.with(OFlags::TRUNC)
    }

    /// Append on every write (`O_APPEND`).
    #[must_use]
    pub const fn append(self) -> Self {
        self.with(OFlags::APPEND)
    }

    const fn with(self, f: OFlags) -> Self {
        Self {
            flags: self.flags.union(f),
            mode: self.mode,
        }
    }

    fn oflags(self) -> OFlags {
        let mut f = self.flags | OFlags::CLOEXEC;
        // openat2 rejects any flag besides these with O_PATH.
        if !f.contains(OFlags::PATH) {
            f |= OFlags::NOCTTY;
        }
        f
    }

    fn rmode(self) -> Mode {
        // openat2 rejects a mode without O_CREAT.
        if self.flags.contains(OFlags::CREATE) {
            Mode::from_raw_mode(self.mode & meta::PERMISSION_BITS)
        } else {
            Mode::empty()
        }
    }
}

/// A directory used as a root: every path below it is resolved by the kernel
/// from this fd, with `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)`.
///
/// `..` stops at the root, absolute symlinks are relative to the root, and
/// magic links are refused, so no path given to a `RootPath` method reaches
/// outside it. Operations that work on a directory entry (create, link,
/// rename, unlink, `statx`, `set_meta`) resolve the parent directory that way
/// and then act on the last name with an `*at` syscall that never follows it.
///
/// Paths may be absolute or relative; both are taken relative to the root.
/// The fd is `O_PATH | O_DIRECTORY | O_CLOEXEC`.
#[derive(Debug)]
pub struct RootPath {
    fd: OwnedFd,
    mnt_id: u64,
}

impl RootPath {
    /// Opens `host_dir` as a root. This is the only host path the crate takes;
    /// it must not contain symlinks (`RESOLVE_NO_SYMLINKS`), so callers pass
    /// a canonical path.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when `openat2` or the `statx` mount id is
    /// missing (Linux before 5.8) or blocked by seccomp; an `ENOSYS` or `EPERM`
    /// here is not retried with a weaker, string-based resolution.
    /// [`Error::Io`] when the directory cannot be opened.
    pub fn open(host_dir: impl AsRef<Path>) -> Result<Self> {
        let host_dir = host_dir.as_ref();
        let fd = rustix::fs::openat2(
            rustix::fs::CWD,
            host_dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|e| match e {
            // An `O_PATH` open checks no permission on the entry itself, so
            // `EPERM` here comes from a seccomp filter, not from the file.
            Errno::NOSYS | Errno::PERM => Error::Unsupported {
                feature: "openat2",
                source: e.into(),
            },
            e => io_err("open root", host_dir, e),
        })?;
        Self::from_dir_fd(fd, host_dir)
    }

    fn from_dir_fd(fd: OwnedFd, path: &Path) -> Result<Self> {
        let st = meta::statx_at(fd.as_fd(), OsStr::new(""), path)?;
        Ok(Self {
            fd,
            mnt_id: st.mnt_id,
        })
    }

    /// The mount id of the root directory.
    #[must_use]
    pub fn mnt_id(&self) -> u64 {
        self.mnt_id
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The same root claiming another mount id: every entry then looks like
    /// it is on another mount. Tests the mount check without mounting.
    #[cfg(test)]
    pub(crate) fn with_mnt_id(mut self, mnt_id: u64) -> Self {
        self.mnt_id = mnt_id;
        self
    }

    /// `openat2` of `p` relative to the root, confined to it. Always
    /// `O_CLOEXEC`.
    pub(crate) fn resolve(&self, p: &Path, flags: OFlags, mode: Mode) -> Result<OwnedFd> {
        self.resolve_in(p, flags, mode, Mounts::Cross)
    }

    /// [`RootPath::resolve`]; with [`Mounts::Stay`], entering another mount
    /// below the root fails with `EXDEV` (`RESOLVE_NO_XDEV`).
    pub(crate) fn resolve_in(
        &self,
        p: &Path,
        flags: OFlags,
        mode: Mode,
        mounts: Mounts,
    ) -> Result<OwnedFd> {
        let rel = path::relative(p);
        let resolve = match mounts {
            Mounts::Cross => IN_ROOT,
            Mounts::Stay => IN_ROOT.union(ResolveFlags::NO_XDEV),
        };
        let flags = flags | OFlags::CLOEXEC;
        openat2_retry(self.fd(), rel.as_os_str(), flags, mode, resolve).map_err(|e| match e {
            Errno::NOSYS => Error::Unsupported {
                feature: "openat2",
                source: e.into(),
            },
            e => io_err("open", p, e),
        })
    }

    /// An `O_PATH` fd of the directory `p`, following symlinks inside the root.
    pub(crate) fn resolve_dir(&self, p: &Path) -> Result<OwnedFd> {
        self.resolve_dir_in(p, Mounts::Cross)
    }

    /// [`RootPath::resolve_dir`], optionally staying on the root's mount.
    pub(crate) fn resolve_dir_in(&self, p: &Path, mounts: Mounts) -> Result<OwnedFd> {
        self.resolve_in(p, OFlags::PATH | OFlags::DIRECTORY, Mode::empty(), mounts)
    }

    /// Opens `p` inside the root.
    ///
    /// Symlinks in `p`, the last one included unless [`OpenHow::no_follow`],
    /// are followed but stay inside the root.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] with the kernel's errno: `ENOENT`, `ELOOP` for a symlink
    /// loop, a magic link or a final symlink with `no_follow`, `EXDEV` never
    /// (a path cannot leave the root, it is clamped).
    pub fn open_at(&self, p: impl AsRef<Path>, how: &OpenHow) -> Result<OwnedFd> {
        self.resolve(p.as_ref(), how.oflags(), how.rmode())
    }

    /// Opens the regular file `p` for reading, following symlinks inside the
    /// root. Use it for any file whose content an image controls.
    ///
    /// The entry is pinned with `O_PATH` and checked before anything opens
    /// it for I/O: opening a FIFO blocks, and opening a device node acts on
    /// the host's device, as root. The pinned inode is then reopened for
    /// reading through ranikust's own `/proc/self/fd`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] when `p` is not a regular file; [`Error::Io`]
    /// as [`RootPath::open_at`].
    pub fn open_regular(&self, p: impl AsRef<Path>) -> Result<OwnedFd> {
        let p = p.as_ref();
        let pinned = self.resolve(p, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
        let file_type = meta::statx_at(pinned.as_fd(), OsStr::new(""), p)?.file_type();
        if file_type != FileType::Regular {
            return Err(Error::InvalidPath {
                path: p.to_owned(),
                reason: "not a regular file",
            });
        }
        let proc = meta::proc_fd_dir(p)?;
        let num = pinned.as_raw_fd().to_string();
        rustix::fs::openat(
            &proc,
            num.as_str(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )
        .map_err(|e| io_err("open", p, e))
    }

    /// Opens the directory `p` as a new root, for example a stage's `rootfs/`
    /// or the source of `COPY --from`. Symlinks are followed inside this root.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `p` is missing or not a directory.
    pub fn sub_root(&self, p: impl AsRef<Path>) -> Result<RootPath> {
        let p = p.as_ref();
        let fd = self.resolve_dir(p)?;
        Self::from_dir_fd(fd, p)
    }

    /// The parent directory of `p` (an `O_PATH` fd) and the last name.
    fn parent<'p>(&self, p: &'p Path) -> Result<(OwnedFd, &'p OsStr)> {
        let (parent, name) = path::entry(p)?;
        Ok((self.resolve_dir(&parent)?, name))
    }

    /// `statx` of `p`, not following a final symlink, with the mount id.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `p` or a parent is missing.
    pub fn statx(&self, p: impl AsRef<Path>) -> Result<Stat> {
        let p = p.as_ref();
        match path::target(p)? {
            Target::Root => meta::statx_at(self.fd(), OsStr::new(""), p),
            Target::Entry { parent, name } => {
                let pfd = self.resolve_dir(&parent)?;
                meta::statx_at(pfd.as_fd(), name, p)
            }
        }
    }

    /// Creates the directory `p` with `mode & !umask`; its parent must exist.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] with `EEXIST` when anything, a symlink included, already
    /// has that name.
    pub fn create_dir(&self, p: impl AsRef<Path>, mode: u32) -> Result<()> {
        let p = p.as_ref();
        let (pfd, name) = self.parent(p)?;
        rustix::fs::mkdirat(
            &pfd,
            name,
            Mode::from_raw_mode(mode & meta::PERMISSION_BITS),
        )
        .map_err(|e| io_err("mkdir", p, e))
    }

    /// Creates `p` and its missing parents with `mode & !umask`, like
    /// `mkdir -p`. Existing symlinks in `p` are followed inside the root.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when a component exists and is not a directory, or is a
    /// symlink that leads nowhere.
    pub fn create_dir_all(&self, p: impl AsRef<Path>, mode: u32) -> Result<()> {
        self.mkdir_all(p.as_ref(), mode, Mounts::Cross).map(drop)
    }

    /// [`RootPath::create_dir_all`], returning an `O_PATH` fd of the directory.
    pub(crate) fn mkdir_all(&self, p: &Path, mode: u32, mounts: Mounts) -> Result<OwnedFd> {
        match self.resolve_dir_in(p, mounts) {
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {}
            r => return r,
        }
        let mode = Mode::from_raw_mode(mode & meta::PERMISSION_BITS);
        let mut cur = self.resolve_dir_in(Path::new("."), mounts)?;
        let mut prefix = PathBuf::new();
        for c in p.components() {
            match c {
                // Only plain names reach `mkdirat`, never `/`.
                Component::RootDir | Component::CurDir | Component::Prefix(_) => continue,
                Component::ParentDir => prefix.push(".."),
                Component::Normal(name) => {
                    prefix.push(name);
                    match rustix::fs::mkdirat(&cur, name, mode) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(io_err("mkdir", &prefix, e)),
                    }
                }
            }
            // Re-resolve from the root: the name may be a symlink, which must
            // be followed relative to the root, not to `cur`.
            cur = self.resolve_dir_in(&prefix, mounts)?;
        }
        Ok(cur)
    }

    /// Creates the symlink `link` pointing to `target`. The target is stored
    /// as given; it is only ever resolved later, inside a root.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] with `EEXIST` when `link` exists.
    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        let link = link.as_ref();
        let (pfd, name) = self.parent(link)?;
        rustix::fs::symlinkat(target.as_ref(), &pfd, name).map_err(|e| io_err("symlink", link, e))
    }

    /// The target of the symlink `p`, as stored.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] with `EINVAL` when `p` is not a symlink.
    pub fn read_link(&self, p: impl AsRef<Path>) -> Result<PathBuf> {
        let p = p.as_ref();
        let (pfd, name) = self.parent(p)?;
        let target =
            rustix::fs::readlinkat(&pfd, name, Vec::new()).map_err(|e| io_err("readlink", p, e))?;
        Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
    }

    /// Creates the hard link `new` to the entry `existing`, both inside this
    /// root. A symlink named by `existing` is linked itself, not followed.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `existing` is missing or `new` exists.
    pub fn hard_link(&self, existing: impl AsRef<Path>, new: impl AsRef<Path>) -> Result<()> {
        let (existing, new) = (existing.as_ref(), new.as_ref());
        let (opfd, oname) = self.parent(existing)?;
        let (npfd, nname) = self.parent(new)?;
        rustix::fs::linkat(&opfd, oname, &npfd, nname, AtFlags::empty())
            .map_err(|e| io_err("link", new, e))
    }

    /// Renames `from` to `to`, both inside this root, replacing `to` as
    /// `rename(2)` does. Neither last name is followed.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] as `rename(2)`.
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
        let (from, to) = (from.as_ref(), to.as_ref());
        let (opfd, oname) = self.parent(from)?;
        let (npfd, nname) = self.parent(to)?;
        rustix::fs::renameat(&opfd, oname, &npfd, nname).map_err(|e| io_err("rename", from, e))
    }

    /// Removes the non-directory `p`; a symlink is removed, not followed.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] with `EISDIR` for a directory.
    pub fn remove_file(&self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        let (pfd, name) = self.parent(p)?;
        rustix::fs::unlinkat(&pfd, name, AtFlags::empty()).map_err(|e| io_err("unlink", p, e))
    }

    /// Removes the empty directory `p`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] with `ENOTEMPTY` when it has entries.
    pub fn remove_dir(&self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        let (pfd, name) = self.parent(p)?;
        rustix::fs::unlinkat(&pfd, name, AtFlags::REMOVEDIR).map_err(|e| io_err("rmdir", p, e))
    }

    /// Every extended attribute of `p`, overlay's own (`user.overlay.*`,
    /// `trusted.overlay.*`) included, without following a final symlink;
    /// sorted by name. A filesystem without xattrs gives none.
    ///
    /// The entry is pinned by an `O_PATH | O_NOFOLLOW` fd and read through
    /// ranikust's own `/proc/self/fd`, so a FIFO or device node is never
    /// opened; a symlink's are read by its name in the pinned parent, and a
    /// swap of that name in between is [`Error::Changed`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `p` or a parent is missing or a read fails.
    pub fn xattrs(&self, p: impl AsRef<Path>) -> Result<Vec<(OsString, Vec<u8>)>> {
        let p = p.as_ref();
        let mut out = match path::target(p)? {
            Target::Root => {
                meta::read_xattrs(self.fd(), FileType::Directory, None, Xattrs::All, p)?
            }
            Target::Entry { parent, name } => {
                let pfd = self.resolve_dir(&parent)?;
                let fd = open_name(
                    pfd.as_fd(),
                    name,
                    OFlags::PATH | OFlags::NOFOLLOW,
                    0,
                    false,
                    p,
                )?;
                let file_type = meta::statx_at(fd.as_fd(), OsStr::new(""), p)?.file_type();
                meta::read_xattrs(
                    fd.as_fd(),
                    file_type,
                    Some((pfd.as_fd(), name)),
                    Xattrs::All,
                    p,
                )?
            }
        };
        out.sort();
        Ok(out)
    }

    /// Sets owner, xattrs, raw mode bits and times of `p` (see [`Meta`] for
    /// the order), without following a final symlink.
    ///
    /// The entry is pinned by an `O_PATH | O_NOFOLLOW` fd before anything is
    /// changed, so a concurrent swap of the name cannot redirect the changes.
    /// Mode and xattrs go through ranikust's own `/proc/self/fd` (checked to
    /// be procfs), because `fchmod` and `fsetxattr` refuse `O_PATH` fds.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] from the first step that fails; earlier steps stay
    /// applied.
    pub fn set_meta(&self, p: impl AsRef<Path>, m: &Meta) -> Result<()> {
        let p = p.as_ref();
        match path::target(p)? {
            Target::Root => {
                let h = Handle::Path {
                    fd: self.fd(),
                    file_type: FileType::Directory,
                    parent: None,
                };
                meta::apply(&h, m, p)
            }
            Target::Entry { parent, name } => {
                let pfd = self.resolve_dir(&parent)?;
                set_meta_at(pfd.as_fd(), name, m, None, p).map(drop)
            }
        }
    }

    /// [`RootPath::set_meta`] only if `p` is a directory, checked on the
    /// pinned entry itself; `Ok(false)` when it is something else. For the
    /// extraction, which never writes across a mount: the parent is resolved
    /// on the root's own mount ([`Mounts::Stay`]).
    pub(crate) fn set_dir_meta(&self, p: &Path, m: &Meta) -> Result<bool> {
        match path::target(p)? {
            Target::Root => self.set_meta(p, m).map(|()| true),
            Target::Entry { parent, name } => {
                let pfd = self.resolve_dir_in(&parent, Mounts::Stay)?;
                set_meta_at(pfd.as_fd(), name, m, Some(FileType::Directory), p)
            }
        }
    }
}

/// [`RootPath::set_meta`] on the entry `name` of the resolved directory
/// `pfd`. With `only`, an entry of another type is left alone and `false`
/// returned; the type is read from the pinned entry, so a swap cannot slip
/// in between the check and the change. `path` is only for error messages.
pub(crate) fn set_meta_at(
    pfd: BorrowedFd<'_>,
    name: &OsStr,
    m: &Meta,
    only: Option<FileType>,
    path: &Path,
) -> Result<bool> {
    let fd = open_name(pfd, name, OFlags::PATH | OFlags::NOFOLLOW, 0, false, path)?;
    let file_type = meta::statx_at(fd.as_fd(), OsStr::new(""), path)?.file_type();
    if only.is_some_and(|t| t != file_type) {
        return Ok(false);
    }
    let h = Handle::Path {
        fd: fd.as_fd(),
        file_type,
        parent: Some((pfd, name)),
    };
    meta::apply(&h, m, path)?;
    Ok(true)
}

/// How often `openat2` is retried on `EAGAIN`.
const EAGAIN_RETRIES: usize = 128;

/// `openat2`, retried on `EAGAIN`.
///
/// With `RESOLVE_IN_ROOT` or `RESOLVE_BENEATH`, the kernel answers `EAGAIN`
/// when a rename or mount anywhere in the system raced with the resolution
/// of a `..` component and it cannot prove the walk stayed inside;
/// `openat2(2)` says the caller may retry. Parallel stages rename all the
/// time, so this is normal, not an attack. The bound only keeps a
/// pathological rename storm from spinning forever.
fn openat2_retry(
    dirfd: BorrowedFd<'_>,
    p: &OsStr,
    flags: OFlags,
    mode: Mode,
    resolve: ResolveFlags,
) -> rustix::io::Result<OwnedFd> {
    let mut tries = 0;
    loop {
        match rustix::fs::openat2(dirfd, p, flags, mode, resolve) {
            Err(Errno::AGAIN) if tries < EAGAIN_RETRIES => tries += 1,
            r => return r,
        }
    }
}

/// Opens the single name `name` in the directory `dirfd`, with no traversal
/// and no symlink followed; with `same_mount`, also without entering another
/// mount (`RESOLVE_NO_XDEV`, `EXDEV`). `path` is only for error messages.
/// `mode` is used only with `O_CREAT`.
pub(crate) fn open_name(
    dirfd: BorrowedFd<'_>,
    name: &OsStr,
    flags: OFlags,
    mode: u32,
    same_mount: bool,
    path: &Path,
) -> Result<OwnedFd> {
    let mode = if flags.contains(OFlags::CREATE) {
        Mode::from_raw_mode(mode & meta::PERMISSION_BITS)
    } else {
        Mode::empty()
    };
    let resolve = if same_mount {
        ONE_NAME.union(ResolveFlags::NO_XDEV)
    } else {
        ONE_NAME
    };
    let flags = flags | OFlags::CLOEXEC;
    openat2_retry(dirfd, name, flags, mode, resolve).map_err(|e| match e {
        Errno::NOSYS => Error::Unsupported {
            feature: "openat2",
            source: e.into(),
        },
        e => io_err("open", path, e),
    })
}

#[cfg(test)]
mod tests;
