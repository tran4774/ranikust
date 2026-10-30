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

    fn to_timespec(self) -> rustix::fs::Timespec {
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
    /// Only `security.capability`, as kaniko's `CopyCapabilities`.
    Capability,
    /// What overlay's copy-up keeps of an inode in an extracted layer: all
    /// but overlay's own (`trusted.overlay.*`, `user.overlay.*`); the
    /// escaped content xattrs `user.overlay.overlay.*` are kept.
    Layer,
}

/// Largest xattr name list or value Linux allows (`XATTR_LIST_MAX`,
/// `XATTR_SIZE_MAX`).
const XATTR_MAX: usize = 64 << 10;
const CAPABILITY: &str = "security.capability";

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
    // A symlink: its name in the pinned parent, with the `l*` calls, which
    // never follow the last name. Anything else: the fd's magic link, with
    // the plain calls, which follow it to exactly the pinned inode.
    let (target, follow) = if file_type == FileType::Symlink {
        let Some((pfd, name)) = parent else {
            return Ok(Vec::new());
        };
        let mut p = OsString::from(proc_path(pfd));
        p.push("/");
        p.push(name);
        (p, false)
    } else {
        (OsString::from(proc_path(fd)), true)
    };
    let list = |buf: &mut [u8]| {
        if follow {
            rustix::fs::listxattr(&target, buf)
        } else {
            rustix::fs::llistxattr(&target, buf)
        }
    };
    let get = |n: &OsStr, buf: &mut [u8]| {
        if follow {
            rustix::fs::getxattr(&target, n, buf)
        } else {
            rustix::fs::lgetxattr(&target, n, buf)
        }
    };
    let mut buf = vec![0; XATTR_MAX];
    let names: Vec<OsString> = match which {
        Xattrs::Capability => vec![OsString::from(CAPABILITY)],
        Xattrs::Layer => match list(&mut buf[..]) {
            Ok(n) => buf[..n]
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| OsStr::from_bytes(s).to_owned())
                .collect(),
            Err(Errno::NOTSUP) => return Ok(Vec::new()),
            Err(e) => return Err(io_err("listxattr", path, e)),
        },
    };
    let mut out = Vec::new();
    for n in names {
        if which == Xattrs::Layer && is_overlay_own(n.as_bytes()) {
            continue;
        }
        match get(n.as_os_str(), &mut buf[..]) {
            Ok(len) => out.push((n, buf[..len].to_vec())),
            Err(Errno::NODATA | Errno::NOTSUP) => {}
            Err(e) => return Err(io_err("getxattr", path, e)),
        }
    }
    if let (false, Some((pfd, name))) = (follow, parent) {
        // The symlink was read by name: check that the name still is the
        // pinned symlink, not an entry swapped in meanwhile.
        let pinned = statx_at(fd, OsStr::new(""), path)?;
        let named = statx_at(pfd, name, path)?;
        if (named.ino, named.dev_major, named.dev_minor)
            != (pinned.ino, pinned.dev_major, pinned.dev_minor)
        {
            return Err(Error::Changed {
                path: path.to_owned(),
            });
        }
    }
    Ok(out)
}

/// An xattr overlayfs itself keeps on a layer's entries (`trusted.overlay.*`,
/// `user.overlay.*`), as opposed to the escaped content xattrs
/// `user.overlay.overlay.*` (`ADR-0016`).
fn is_overlay_own(name: &[u8]) -> bool {
    if name.starts_with(b"user.overlay.overlay.") {
        return false;
    }
    name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.")
}

/// Applies `m` to the entry behind `h`. `path` is only for error messages.
pub(crate) fn apply(h: &Handle<'_>, m: &Meta, path: &Path) -> Result<()> {
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
                rustix::fs::fsetxattr(fd, name.as_os_str(), value, XattrFlags::empty())
                    .map_err(|e| io_err("setxattr", path, e))?;
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
                let target = if file_type == FileType::Symlink {
                    // `setxattr` through the fd's magic link would follow
                    // the symlink; name it in its pinned parent instead and
                    // use `lsetxattr`, which never follows the last name.
                    let Some((pfd, name)) = parent else {
                        return Err(Error::InvalidPath {
                            path: path.to_owned(),
                            reason: "a symlink root has no parent to set xattrs through",
                        });
                    };
                    let mut p = OsString::from(proc_path(pfd));
                    p.push("/");
                    p.push(name);
                    p
                } else {
                    OsString::from(proc_path(fd))
                };
                for (name, value) in &m.xattrs {
                    let r = if file_type == FileType::Symlink {
                        rustix::fs::lsetxattr(&target, name.as_os_str(), value, XattrFlags::empty())
                    } else {
                        rustix::fs::setxattr(&target, name.as_os_str(), value, XattrFlags::empty())
                    };
                    r.map_err(|e| io_err("setxattr", path, e))?;
                }
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
