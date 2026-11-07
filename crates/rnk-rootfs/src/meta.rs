//! File metadata: what `statx` reports and what `set_meta` writes.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{
    AtFlags, FileType as RawFileType, Gid, Mode, OFlags, PROC_SUPER_MAGIC, Statx, StatxAttributes,
    StatxFlags, Timestamps, Uid, XattrFlags,
};
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};

/// Permission bits of `st_mode`, including setuid, setgid and sticky.
pub(crate) const PERMISSION_BITS: u32 = 0o7777;

/// A point in time: seconds and nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTime {
    /// Seconds since 1970-01-01T00:00:00Z; negative before.
    pub secs: i64,
    /// Nanoseconds, below 1,000,000,000.
    pub nanos: u32,
}

impl FileTime {
    /// A time with whole seconds.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self { secs, nanos: 0 }
    }

    pub(crate) fn to_timespec(self) -> rustix::fs::Timespec {
        rustix::fs::Timespec {
            tv_sec: self.secs,
            tv_nsec: i64::from(self.nanos),
        }
    }
}

/// The type of a directory entry, from the `S_IFMT` bits of `st_mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileType {
    /// A regular file.
    Regular,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A character device (an overlay whiteout is the device 0/0).
    CharDevice,
    /// A block device.
    BlockDevice,
    /// A named pipe.
    Fifo,
    /// A socket.
    Socket,
    /// Any other value of the type bits.
    Unknown,
}

impl FileType {
    pub(crate) fn from_mode(mode: u32) -> Self {
        match RawFileType::from_raw_mode(mode) {
            RawFileType::RegularFile => Self::Regular,
            RawFileType::Directory => Self::Directory,
            RawFileType::Symlink => Self::Symlink,
            RawFileType::CharacterDevice => Self::CharDevice,
            RawFileType::BlockDevice => Self::BlockDevice,
            RawFileType::Fifo => Self::Fifo,
            RawFileType::Socket => Self::Socket,
            RawFileType::Unknown => Self::Unknown,
        }
    }
}

/// What `statx` reports about an entry, without following a final symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stat {
    /// The raw `st_mode`: type and permission bits.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Number of hard links.
    pub nlink: u32,
    /// Inode number.
    pub ino: u64,
    /// Major number of the device holding the entry.
    pub dev_major: u32,
    /// Minor number of the device holding the entry.
    pub dev_minor: u32,
    /// Major number of a device node.
    pub rdev_major: u32,
    /// Minor number of a device node.
    pub rdev_minor: u32,
    /// Size in bytes.
    pub size: u64,
    /// Last access.
    pub atime: FileTime,
    /// Last modification of the content.
    pub mtime: FileTime,
    /// Last change of the inode.
    pub ctime: FileTime,
    /// Mount id (`stx_mnt_id`): which mount the entry is on. Compare this,
    /// never `st_dev` alone, to tell whether two entries share a mount.
    pub mnt_id: u64,
    /// `STATX_ATTR_MOUNT_ROOT`: the entry is the root of a mount.
    pub mount_root: bool,
}

impl Stat {
    /// The entry type.
    #[must_use]
    pub fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode)
    }

    /// The permission bits, setuid, setgid and sticky included.
    #[must_use]
    pub fn permissions(&self) -> u32 {
        self.mode & PERMISSION_BITS
    }

    /// Whether `other` describes the same inode: the same inode number on
    /// the same device.
    pub(crate) fn same_inode(&self, other: &Stat) -> bool {
        (self.ino, self.dev_major, self.dev_minor) == (other.ino, other.dev_major, other.dev_minor)
    }

    /// Whether `other` describes the same entry: the same inode, reached on
    /// the same mount. Whether either is a mount point (`mount_root`) is not
    /// compared: callers that need it check it themselves.
    pub(crate) fn same_entry(&self, other: &Stat) -> bool {
        self.same_inode(other) && self.mnt_id == other.mnt_id
    }

    fn from_statx(x: &Statx) -> Self {
        let t = |ts: rustix::fs::StatxTimestamp| FileTime {
            secs: ts.tv_sec,
            nanos: ts.tv_nsec,
        };
        Self {
            mode: u32::from(x.stx_mode),
            uid: x.stx_uid,
            gid: x.stx_gid,
            nlink: x.stx_nlink,
            ino: x.stx_ino,
            dev_major: x.stx_dev_major,
            dev_minor: x.stx_dev_minor,
            rdev_major: x.stx_rdev_major,
            rdev_minor: x.stx_rdev_minor,
            size: x.stx_size,
            atime: t(x.stx_atime),
            mtime: t(x.stx_mtime),
            ctime: t(x.stx_ctime),
            mnt_id: x.stx_mnt_id,
            mount_root: x.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT)
                && x.stx_attributes.contains(StatxAttributes::MOUNT_ROOT),
        }
    }
}

/// `statx(dirfd, name, AT_SYMLINK_NOFOLLOW)`, or of `dirfd` itself when
/// `name` is empty, requiring the mount id.
pub(crate) fn statx_at(dirfd: BorrowedFd<'_>, name: &std::ffi::OsStr, path: &Path) -> Result<Stat> {
    let mut flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
    if name.is_empty() {
        flags |= AtFlags::EMPTY_PATH;
    }
    let x = rustix::fs::statx(
        dirfd,
        name,
        flags,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    )
    .map_err(|e| match e {
        Errno::NOSYS | Errno::PERM => Error::Unsupported {
            feature: "statx",
            source: e.into(),
        },
        e => io_err("statx", path, e),
    })?;
    if !StatxFlags::from_bits_retain(x.stx_mask).contains(StatxFlags::MNT_ID) {
        return Err(Error::Unsupported {
            feature: "statx mount id (STATX_MNT_ID)",
            source: std::io::Error::from(std::io::ErrorKind::Unsupported),
        });
    }
    Ok(Stat::from_statx(&x))
}

/// Metadata to set on an entry. `None` fields and an empty xattr list leave
/// the entry unchanged.
///
/// Applied in this order, because each step can undo an earlier one: owner
/// (`chown` clears setuid, setgid and `security.capability`), xattrs (setting
/// `user.*` needs write permission, which the mode may then remove), mode,
/// times (every other change touches `ctime`, and the times come last so
/// nothing moves them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// New owner; set with `fchownat(AT_SYMLINK_NOFOLLOW)`.
    pub uid: Option<u32>,
    /// New group; set with `fchownat(AT_SYMLINK_NOFOLLOW)`.
    pub gid: Option<u32>,
    /// Raw permission bits (`& 0o7777`): setuid, setgid and sticky are kept,
    /// type bits ignored, umask not applied. Ignored for symlinks, whose mode
    /// Linux does not use.
    pub mode: Option<u32>,
    /// Last access time.
    pub atime: Option<FileTime>,
    /// Last modification time.
    pub mtime: Option<FileTime>,
    /// Extended attributes to set, `(name, value)`, for example
    /// `("security.capability", ...)`. Existing ones not listed are kept.
    pub xattrs: Vec<(OsString, Vec<u8>)>,
}

/// The entry `set_meta` works on.
pub(crate) enum Handle<'a> {
    /// An fd opened for I/O (a regular file being extracted): plain `f*`
    /// syscalls work on it.
    Open(BorrowedFd<'a>),
    /// An `O_PATH` fd pinning the entry. `parent`/`name` locate it again for
    /// the one operation that needs a path (xattrs on a symlink); `None` for
    /// the root itself.
    Path {
        fd: BorrowedFd<'a>,
        file_type: FileType,
        parent: Option<(BorrowedFd<'a>, &'a std::ffi::OsStr)>,
    },
}

/// The process's fd directory in procfs, checked to really be procfs.
///
/// Used to change the mode of an entry held by an `O_PATH` fd (`fchmod`
/// refuses `O_PATH`; `fchmodat` follows the magic link to exactly that inode,
/// as glibc's `fchmodat(AT_SYMLINK_NOFOLLOW)` emulation does) and to set its
/// xattrs. These are ranikust's own fds in ranikust's own `/proc`, never a
/// path inside a root.
pub(crate) fn proc_fd_dir(path: &Path) -> Result<OwnedFd> {
    let fd = rustix::fs::open(
        "/proc/self/fd",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::Unsupported {
        feature: "/proc/self/fd",
        source: e.into(),
    })?;
    let fs = rustix::fs::fstatfs(&fd).map_err(|e| io_err("fstatfs /proc/self/fd", path, e))?;
    if fs.f_type != PROC_SUPER_MAGIC {
        return Err(Error::Unsupported {
            feature: "procfs at /proc",
            source: std::io::Error::other("/proc/self/fd is not on procfs"),
        });
    }
    Ok(fd)
}

fn proc_path(fd: BorrowedFd<'_>) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// Reopens the entry pinned by the `O_PATH` fd `pinned` with `flags`
/// (`O_RDONLY`, plus `O_DIRECTORY` for a directory) through ranikust's own
/// `/proc/self/fd`: exactly that inode, whatever its name points to now.
/// The caller checks the type on the pinned fd first, so a FIFO or a device
/// node is never opened. `path` is only for error messages.
pub(crate) fn reopen(pinned: BorrowedFd<'_>, flags: OFlags, path: &Path) -> Result<OwnedFd> {
    let proc = proc_fd_dir(path)?;
    let num = pinned.as_raw_fd().to_string();
    rustix::fs::openat(
        &proc,
        num.as_str(),
        flags | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(|e| io_err("open", path, e))
}

/// Which extended attributes [`read_xattrs`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Xattrs {
    /// What overlay's copy-up keeps of an inode in an extracted layer: all
    /// but overlay's own (`trusted.overlay.*`, `user.overlay.*`) and every
    /// other `trusted.*`, which no layer gets ([`LAYER_SKIPPED_XATTR`]);
    /// the escaped content xattrs `user.overlay.overlay.*` are kept.
    Layer,
    /// Every xattr, overlay's own included: for code that reads overlay's
    /// markers (`rnk-snapshot`'s exporter).
    All,
    /// What `COPY` carries over, as BuildKit copies every xattr (Q-120):
    /// all but overlay's own, set best effort ([`apply_lenient`]).
    Copy,
}

impl Xattrs {
    /// Whether the xattr `name` is among these.
    fn keeps(self, name: &[u8]) -> bool {
        match self {
            Self::All => true,
            Self::Layer => !is_overlay_own(name) && !name.starts_with(LAYER_SKIPPED_XATTR),
            Self::Copy => !is_overlay_own(name),
        }
    }
}

/// Xattrs no extracted layer gets (`ADR-0075`), from its tar or copied up
/// from a lower layer: a filesystem may give them a meaning it leaves to
/// the privileged caller to check, such as XFS's raw on-disk ACLs
/// `trusted.SGI_ACL_FILE` and `trusted.SGI_ACL_DEFAULT`. containerd's
/// `archive.Apply`, BuildKit's layer extraction, refuses the same prefix
/// before the syscall (`buildkit@cd8ee244
/// vendor/github.com/containerd/containerd/v2/pkg/archive/tar_unix.go:146-152`).
pub(crate) const LAYER_SKIPPED_XATTR: &[u8] = b"trusted.";

/// Largest xattr name list or value Linux allows (`XATTR_LIST_MAX`,
/// `XATTR_SIZE_MAX`).
pub(crate) const XATTR_MAX: usize = 64 << 10;

/// The extended attributes of the pinned entry `fd` of type `file_type`.
///
/// Read as [`apply`] writes them: through the fd's magic link in ranikust's
/// own `/proc/self/fd`, except for a symlink, which is named in its pinned
/// parent `parent` (`lgetxattr` never follows the last name); a symlink
/// without a parent has none. A filesystem without xattrs (`ENOTSUP`) has
/// none. `path` is only for error messages.
pub(crate) fn read_xattrs(
    fd: BorrowedFd<'_>,
    file_type: FileType,
    parent: Option<(BorrowedFd<'_>, &OsStr)>,
    which: Xattrs,
    path: &Path,
) -> Result<Vec<(OsString, Vec<u8>)>> {
    proc_fd_dir(path)?;
    let Some(target) = XattrTarget::of(fd, file_type, parent) else {
        return Ok(Vec::new());
    };
    let mut buf = vec![0; XATTR_MAX];
    let names: Vec<OsString> = match target.list(&mut buf[..]) {
        Ok(n) => buf[..n]
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| OsStr::from_bytes(s).to_owned())
            .collect(),
        Err(Errno::NOTSUP) => return Ok(Vec::new()),
        Err(e) => return Err(io_err("listxattr", path, e)),
    };
    let mut out = Vec::new();
    for n in names {
        if !which.keeps(n.as_bytes()) {
            continue;
        }
        match target.get(n.as_os_str(), &mut buf[..]) {
            Ok(len) => out.push((n, buf[..len].to_vec())),
            Err(Errno::NODATA | Errno::NOTSUP) => {}
            Err(e) => return Err(io_err("getxattr", path, e)),
        }
    }
    // After the reads, so a swap during them is caught too.
    target.check_still_pinned(path)?;
    Ok(out)
}

/// How [`read_xattrs`], [`strip_overlay_own`] and [`apply`] reach a pinned
/// entry's xattrs. The variant picks the calls, so a symlink's name is never
/// given to a call that follows it.
enum XattrTarget<'a> {
    /// The fd's magic link in ranikust's own `/proc/self/fd`, with the plain
    /// calls, which follow it to exactly the pinned inode.
    MagicLink(OsString),
    /// A symlink: its name in its pinned parent, with the `l*` calls, which
    /// never follow the last name; [`XattrTarget::check_still_pinned`]
    /// checks the name against `pinned`.
    Named {
        target: OsString,
        pinned: BorrowedFd<'a>,
        parent: BorrowedFd<'a>,
        name: &'a OsStr,
    },
}

impl<'a> XattrTarget<'a> {
    /// The target of the pinned entry `fd` of type `file_type`; `None` for a
    /// symlink without a pinned parent, whose xattrs cannot be reached.
    fn of(
        fd: BorrowedFd<'a>,
        file_type: FileType,
        parent: Option<(BorrowedFd<'a>, &'a OsStr)>,
    ) -> Option<Self> {
        if file_type != FileType::Symlink {
            return Some(Self::MagicLink(OsString::from(proc_path(fd))));
        }
        let (pfd, name) = parent?;
        let mut target = OsString::from(proc_path(pfd));
        target.push("/");
        target.push(name);
        Some(Self::Named {
            target,
            pinned: fd,
            parent: pfd,
            name,
        })
    }

    fn list(&self, buf: &mut [u8]) -> rustix::io::Result<usize> {
        match self {
            Self::MagicLink(target) => rustix::fs::listxattr(target, buf),
            Self::Named { target, .. } => rustix::fs::llistxattr(target, buf),
        }
    }

    fn get(&self, n: &OsStr, buf: &mut [u8]) -> rustix::io::Result<usize> {
        match self {
            Self::MagicLink(target) => rustix::fs::getxattr(target, n, buf),
            Self::Named { target, .. } => rustix::fs::lgetxattr(target, n, buf),
        }
    }

    fn remove(&self, n: &OsStr) -> rustix::io::Result<()> {
        match self {
            Self::MagicLink(target) => rustix::fs::removexattr(target, n),
            Self::Named { target, .. } => rustix::fs::lremovexattr(target, n),
        }
    }

    fn set(&self, n: &OsStr, value: &[u8]) -> rustix::io::Result<()> {
        match self {
            Self::MagicLink(target) => rustix::fs::setxattr(target, n, value, XattrFlags::empty()),
            Self::Named { target, .. } => {
                rustix::fs::lsetxattr(target, n, value, XattrFlags::empty())
            }
        }
    }

    /// For a symlink reached by name: that the name still is the pinned
    /// symlink, not an entry swapped in meanwhile. The magic link reaches
    /// exactly the pinned inode, so it has nothing to check.
    fn check_still_pinned(&self, path: &Path) -> Result<()> {
        match self {
            Self::MagicLink(_) => Ok(()),
            Self::Named {
                pinned,
                parent,
                name,
                ..
            } => {
                let held = statx_at(*pinned, OsStr::new(""), path)?;
                let named = statx_at(*parent, name, path)?;
                if !named.same_inode(&held) {
                    return Err(Error::Changed {
                        path: path.to_owned(),
                    });
                }
                Ok(())
            }
        }
    }
}

/// An xattr overlayfs itself keeps on a layer's entries (`trusted.overlay.*`,
/// `user.overlay.*`), as opposed to the escaped content xattrs
/// `user.overlay.overlay.*` (`ADR-0016`): what a layer and a copy leave out
/// (`Xattrs::Layer`, `Xattrs::Copy`) and [`crate::prune`] removes.
#[must_use]
pub fn is_overlay_own(name: &[u8]) -> bool {
    if name.starts_with(b"user.overlay.overlay.") {
        return false;
    }
    name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.")
}

/// Removes overlay's own xattrs ([`is_overlay_own`]) from the pinned entry
/// `fd` of type `file_type`, reaching it as [`read_xattrs`] does: through
/// the fd's magic link, or a symlink by its name in its pinned parent with
/// the `l*` calls (a symlink without a parent is left alone), checked,
/// before anything is removed, to still be the pinned one. No time a layer
/// holds changes.
/// `path` is only for error messages.
pub(crate) fn strip_overlay_own(
    fd: BorrowedFd<'_>,
    file_type: FileType,
    parent: Option<(BorrowedFd<'_>, &OsStr)>,
    path: &Path,
) -> Result<()> {
    proc_fd_dir(path)?;
    let Some(target) = XattrTarget::of(fd, file_type, parent) else {
        return Ok(());
    };
    let mut buf = vec![0; XATTR_MAX];
    let names: Vec<OsString> = match target.list(&mut buf[..]) {
        Ok(n) => buf[..n]
            .split(|&b| b == 0)
            .filter(|s| is_overlay_own(s))
            .map(|s| OsStr::from_bytes(s).to_owned())
            .collect(),
        Err(Errno::NOTSUP) => return Ok(()),
        Err(e) => return Err(io_err("listxattr", path, e)),
    };
    if names.is_empty() {
        return Ok(());
    }
    // Before the removals: they change the entry.
    target.check_still_pinned(path)?;
    for n in names {
        match target.remove(n.as_os_str()) {
            Ok(()) | Err(Errno::NODATA) => {}
            Err(e) => return Err(io_err("removexattr", path, e)),
        }
    }
    Ok(())
}

/// Applies `m` to the entry behind `h`. `path` is only for error messages.
pub(crate) fn apply(h: &Handle<'_>, m: &Meta, path: &Path) -> Result<()> {
    apply_with(h, m, path, None)
}

/// [`apply`], but an xattr the entry refuses (a `security.*` name the
/// filesystem or the process may not set) is left out and counted in
/// `skipped`, as BuildKit's copy only logs it (`buildkit@cd8ee244
/// solver/llbsolver/file/backend.go:262-265`).
pub(crate) fn apply_lenient(
    h: &Handle<'_>,
    m: &Meta,
    path: &Path,
    skipped: &SkippedXattrs,
) -> Result<()> {
    apply_with(h, m, path, Some(skipped))
}

/// The xattrs one copy left out, reported once when it ends
/// ([`SkippedXattrs::warn`]) rather than once per file.
#[derive(Debug, Default)]
pub(crate) struct SkippedXattrs {
    count: std::cell::Cell<u64>,
    first: std::cell::RefCell<Option<String>>,
}

impl SkippedXattrs {
    /// Records one xattr (or one entry's list) that was not copied.
    pub(crate) fn note(&self, path: &Path, what: &str, e: &dyn std::fmt::Display) {
        self.count.set(self.count.get() + 1);
        let mut first = self.first.borrow_mut();
        if first.is_none() {
            *first = Some(format!("{}: {what}: {e}", path.display()));
        }
    }

    /// One warning for the whole copy, with the count and the first.
    pub(crate) fn warn(&self) {
        if let Some(first) = self.first.borrow().as_ref() {
            tracing::warn!(
                "{} xattr(s) not copied, as BuildKit leaves them out; the first: {first}",
                self.count.get()
            );
        }
    }
}

/// [`read_xattrs`] with [`Xattrs::Copy`], for a copy: an entry whose xattrs
/// cannot be listed or read has none copied, counted in `skipped`, as
/// BuildKit only logs it. Any other error, such as a swapped name, stands.
pub(crate) fn read_xattrs_lenient(
    fd: BorrowedFd<'_>,
    file_type: FileType,
    parent: Option<(BorrowedFd<'_>, &OsStr)>,
    path: &Path,
    skipped: &SkippedXattrs,
) -> Result<Vec<(OsString, Vec<u8>)>> {
    match read_xattrs(fd, file_type, parent, Xattrs::Copy, path) {
        Err(
            e @ Error::Io {
                op: "listxattr" | "getxattr",
                ..
            },
        ) => {
            skipped.note(path, "xattrs", &e);
            Ok(Vec::new())
        }
        r => r,
    }
}

/// What a refused xattr does: an error when strict (no `skipped`), else it
/// is counted.
fn xattr_result(
    r: rustix::io::Result<()>,
    name: &OsStr,
    skipped: Option<&SkippedXattrs>,
    path: &Path,
) -> Result<()> {
    match (r, skipped) {
        (Ok(()), _) => Ok(()),
        (Err(e), None) => Err(io_err("setxattr", path, e)),
        (Err(e), Some(s)) => {
            s.note(path, &format!("xattr {}", name.to_string_lossy()), &e);
            Ok(())
        }
    }
}

fn apply_with(
    h: &Handle<'_>,
    m: &Meta,
    path: &Path,
    skipped: Option<&SkippedXattrs>,
) -> Result<()> {
    let owner_changes = m.uid.is_some() || m.gid.is_some();
    let uid = m.uid.map(Uid::from_raw);
    let gid = m.gid.map(Gid::from_raw);
    let mode = m
        .mode
        .map(|bits| Mode::from_raw_mode(bits & PERMISSION_BITS));
    let times = (m.atime.is_some() || m.mtime.is_some()).then(|| {
        let omit = rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        };
        Timestamps {
            last_access: m.atime.map_or(omit, FileTime::to_timespec),
            last_modification: m.mtime.map_or(omit, FileTime::to_timespec),
        }
    });

    match *h {
        Handle::Open(fd) => {
            if owner_changes {
                rustix::fs::fchown(fd, uid, gid).map_err(|e| io_err("chown", path, e))?;
            }
            for (name, value) in &m.xattrs {
                let r = rustix::fs::fsetxattr(fd, name.as_os_str(), value, XattrFlags::empty());
                xattr_result(r, name, skipped, path)?;
            }
            if let Some(mode) = mode {
                rustix::fs::fchmod(fd, mode).map_err(|e| io_err("chmod", path, e))?;
            }
            if let Some(times) = times {
                rustix::fs::futimens(fd, &times).map_err(|e| io_err("utimensat", path, e))?;
            }
        }
        Handle::Path {
            fd,
            file_type,
            parent,
        } => {
            let empty = AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW;
            if owner_changes {
                rustix::fs::chownat(fd, "", uid, gid, empty)
                    .map_err(|e| io_err("chown", path, e))?;
            }
            if !m.xattrs.is_empty() {
                // Validated before the path through /proc is used.
                proc_fd_dir(path)?;
                // `setxattr` through the fd's magic link would follow a
                // symlink: a symlink is named in its pinned parent instead,
                // with `lsetxattr`, which never follows the last name.
                let Some(target) = XattrTarget::of(fd, file_type, parent) else {
                    return Err(Error::InvalidPath {
                        path: path.to_owned(),
                        reason: "a symlink root has no parent to set xattrs through",
                    });
                };
                // A symlink is set by name: the name is checked to still be
                // the pinned symlink before the sets and again after them.
                // That detects a swap and narrows the window, but does not
                // prevent the write: an entry swapped in between a check
                // and a set gets the xattr (one in the same pinned parent,
                // never through a symlink), and a swap undone before the
                // second check is not seen. `setxattrat(2)` (Linux 6.13)
                // with `AT_EMPTY_PATH` on the `O_PATH` fd might set it
                // race-free, if the kernel accepts an `O_PATH` fd there
                // (unverified; to settle with poc-runner in the hardening
                // pass).
                target.check_still_pinned(path)?;
                for (name, value) in &m.xattrs {
                    xattr_result(target.set(name, value), name, skipped, path)?;
                }
                target.check_still_pinned(path)?;
            }
            if let Some(mode) = mode
                && file_type != FileType::Symlink
            {
                let proc = proc_fd_dir(path)?;
                let num = fd.as_raw_fd().to_string();
                rustix::fs::chmodat(&proc, num.as_str(), mode, AtFlags::empty())
                    .map_err(|e| io_err("chmod", path, e))?;
            }
            if let Some(times) = times {
                rustix::fs::utimensat(fd, "", &times, empty)
                    .map_err(|e| io_err("utimensat", path, e))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    /// A layer's copy-up leaves out what extraction never sets
    /// (`trusted.*`, `ADR-0075`), so a lower layer extracted earlier, or a
    /// filesystem that lists its own `trusted.*` to root, cannot pass one
    /// on; `COPY` keeps every xattr but overlay's own (Q-120).
    #[test]
    fn which_xattrs_a_layer_and_a_copy_keep() {
        for (name, layer, copy) in [
            (&b"user.app"[..], true, true),
            (b"security.capability", true, true),
            (b"system.posix_acl_access", true, true),
            (b"user.overlay.overlay.origin", true, true),
            (b"user.overlay.origin", false, false),
            (b"trusted.overlay.origin", false, false),
            (b"trusted.SGI_ACL_FILE", false, true),
            (b"trusted.x", false, true),
        ] {
            let n = String::from_utf8_lossy(name);
            assert_eq!(Xattrs::Layer.keeps(name), layer, "{n}");
            assert_eq!(Xattrs::Copy.keeps(name), copy, "{n}");
            assert!(Xattrs::All.keeps(name), "{n}");
        }
    }

    #[test]
    fn skipped_xattrs_count_all_and_keep_the_first() {
        let s = SkippedXattrs::default();
        s.note(Path::new("a"), "xattr security.selinux", &"EPERM");
        s.note(Path::new("b"), "xattr trusted.x", &"EPERM");
        assert_eq!(s.count.get(), 2);
        assert_eq!(
            s.first.borrow().as_deref(),
            Some("a: xattr security.selinux: EPERM")
        );
    }

    /// An `O_PATH` fd pinning `p` itself, never what it points to.
    fn pin(p: &Path, flags: OFlags) -> OwnedFd {
        rustix::fs::openat(
            rustix::fs::CWD,
            p,
            flags | OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    /// A pinned symlink, its pinned parent, and a new symlink renamed over
    /// its name: what a swap between pinning and the xattr calls leaves.
    fn swapped_symlink() -> (tempfile::TempDir, OwnedFd, OwnedFd) {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("l")).unwrap();
        let parent = pin(dir.path(), OFlags::DIRECTORY);
        let fd = pin(&dir.path().join("l"), OFlags::empty());
        std::os::unix::fs::symlink("b", dir.path().join("new")).unwrap();
        std::fs::rename(dir.path().join("new"), dir.path().join("l")).unwrap();
        (dir, parent, fd)
    }

    /// A symlink's xattrs are read by its name, then the name is checked to
    /// still be the pinned symlink.
    #[test]
    fn read_xattrs_of_a_symlink_whose_name_was_swapped_is_changed() {
        let (_dir, parent, fd) = swapped_symlink();
        let parent = Some((parent.as_fd(), OsStr::new("l")));
        let e = read_xattrs(
            fd.as_fd(),
            FileType::Symlink,
            parent,
            Xattrs::All,
            Path::new("l"),
        )
        .unwrap_err();
        assert!(matches!(e, Error::Changed { .. }), "{e:?}");
    }

    /// Stripping checks the name only before it removes something: a
    /// symlink with none of overlay's own xattrs is left as it is.
    #[test]
    fn strip_overlay_own_of_a_swapped_symlink_with_nothing_to_remove_is_ok() {
        let (_dir, parent, fd) = swapped_symlink();
        let parent = Some((parent.as_fd(), OsStr::new("l")));
        strip_overlay_own(fd.as_fd(), FileType::Symlink, parent, Path::new("l")).unwrap();
    }

    /// A symlink without a pinned parent has no xattrs to reach.
    #[test]
    fn a_symlink_without_a_parent_has_no_xattrs_and_nothing_stripped() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("l")).unwrap();
        let fd = pin(&dir.path().join("l"), OFlags::empty());
        let read = read_xattrs(
            fd.as_fd(),
            FileType::Symlink,
            None,
            Xattrs::All,
            Path::new("l"),
        );
        assert!(read.unwrap().is_empty());
        strip_overlay_own(fd.as_fd(), FileType::Symlink, None, Path::new("l")).unwrap();
    }

    /// Setting a symlink's xattrs by name checks first that the name still
    /// is the pinned symlink: nothing is set on an entry swapped in.
    #[test]
    fn apply_xattrs_to_a_symlink_whose_name_was_swapped_is_changed() {
        let (_dir, parent, fd) = swapped_symlink();
        let h = Handle::Path {
            fd: fd.as_fd(),
            file_type: FileType::Symlink,
            parent: Some((parent.as_fd(), OsStr::new("l"))),
        };
        let m = Meta {
            xattrs: vec![(OsString::from("user.x"), b"y".to_vec())],
            ..Meta::default()
        };
        let e = apply(&h, &m, Path::new("l")).unwrap_err();
        assert!(matches!(e, Error::Changed { .. }), "{e:?}");
    }
}
