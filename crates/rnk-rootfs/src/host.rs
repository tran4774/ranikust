//! Host paths the operator names, reached so that the build context's owner
//! cannot redirect them (ADR-0076).
//!
//! The build context belongs to a principal of its own, trusted less than
//! the operator: whoever owns the context directory, or commits to the
//! repository checked out there, controls every name inside it, symlinks
//! and hardlinks included. So:
//!
//! - an output is written in an [`OutputDir`], reached from `/` one name at
//!   a time, never through a symlink and never into the build context,
//!   its missing directories made by descriptor below the last one reached;
//! - a path ranikust reads, such as `--dockerfile`, is walked by [`reach`]:
//!   symlinks are followed while the walk is outside the context, and once
//!   it enters the context the rest is the context's, resolved through the
//!   context's own [`RootPath`] by the caller.
//!
//! The context is told by its root directory's inode ([`Fence`]), so a bind
//! mount of it elsewhere counts as the context too.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};
use crate::meta::{self, FileType, Stat};
use crate::root::{self, Pinned, RootPath};

/// Most symlinks [`reach`] follows in one path, as the kernel's
/// `MAXSYMLINKS`.
const MAX_SYMLINKS: usize = 40;

/// How many temporary names [`OutputDir::write_atomically`] tries before it
/// gives up: each is random, so a second try is already unlikely.
const TMP_TRIES: usize = 16;

/// Why an output path is refused when it leads into the build context.
const IN_CONTEXT: &str = "lies in the build context, whose owner controls every name in it; \
     put the output outside the context (ADR-0076)";

/// Why an output path is refused when a component is a symlink.
const THROUGH_SYMLINK: &str = "is or passes through a symlink, and ranikust follows none in an \
     output path (ADR-0076); pass the path without symlinks, as `realpath` prints it";

/// `statfs` magic numbers of kernel-interface filesystems, which
/// [`OutputDir::write_atomically`] refuses: proc, sysfs, devpts, cgroup,
/// cgroup2, securityfs, debugfs, tracefs, bpf, selinuxfs, configfs,
/// efivarfs, pstore, fusectl. devtmpfs reports tmpfs's number and cannot be
/// told apart; the refusal of a name that is no regular file covers it.
const KERNEL_FS: [i128; 14] = [
    0x9fa0,
    0x6265_6572,
    0x1cd1,
    0x0027_e0eb,
    0x6367_7270,
    0x7363_6673,
    0x6462_6720,
    0x7472_6163,
    0xcafe_4a11,
    0xf97c_ff8c,
    0x6265_6570,
    0xde5e_81e4,
    0x6165_676c,
    0x6573_5543,
];

/// Why [`reach`] refuses a symlink outside the context.
const UNTRUSTED_LINK: &str = "is a symlink ranikust does not follow (ADR-0076): owned by an \
     account other than root and ranikust's own, hard-linked elsewhere, or in a directory such an \
     account owns or that its group or others may write; pass the path it points to";

/// Whether [`reach`] follows the symlink `link`, found in the directory
/// `dir`, outside the context. Both must be root's or ranikust's own user's,
/// the link must have no other name (a symlink hard-linked in from
/// elsewhere, which `fs.protected_hardlinks=0` allows), and the directory
/// must not be writable by its group or others, so no other account could
/// have placed or moved the link there. ranikust builds as root, so a link
/// a lower-trust account made or placed is never followed; a link committed
/// to a checkout root made is, which is why a parse error never shows a
/// file's text.
fn trusted_link(link: &Stat, dir: &Stat) -> bool {
    let euid = rustix::process::geteuid().as_raw();
    let ours = |uid: u32| uid == 0 || uid == euid;
    ours(link.uid) && link.nlink == 1 && ours(dir.uid) && dir.mode & 0o022 == 0
}

/// The build context's root directory, which an operator's host path must
/// not lead into: the inode, so a bind mount of the context is the context
/// too.
#[derive(Clone, Copy, Debug)]
pub struct Fence {
    root: Stat,
}

impl Fence {
    /// The fence of the root `root`, the build context's.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `statx` fails on the root's descriptor.
    pub fn of(root: &RootPath) -> Result<Self> {
        Ok(Self {
            root: meta::statx_at(root.fd(), OsStr::new(""), Path::new(""))?,
        })
    }

    /// Whether the directory `st` is the fenced one.
    fn is(&self, st: &Stat) -> bool {
        self.root.same_inode(st)
    }

    /// The owner of the fenced directory: the build context's owner.
    #[must_use]
    pub fn owner(&self) -> u32 {
        self.root.uid
    }
}

/// Where [`reach`] led.
#[derive(Debug)]
pub enum Reached {
    /// An entry outside the build context, pinned with the directory it is
    /// in.
    Outside(HostFile),
    /// The path enters the build context: the rest of it, relative to the
    /// context's root, for the caller to resolve through the context's
    /// [`RootPath`]. Empty when the path names the context's root itself.
    Inside(PathBuf),
}

/// An entry [`reach`] found outside the build context: pinned
/// `O_PATH | O_NOFOLLOW`, never a symlink, with the directory it is in, so a
/// name beside it is reached from that directory, not by path again.
#[derive(Debug)]
pub struct HostFile {
    entry: Pinned,
    dir: OwnedFd,
}

impl HostFile {
    /// The path walked, symlinks replaced by what they pointed to.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.entry.path()
    }

    /// What `statx` said of the entry.
    #[must_use]
    pub fn stat(&self) -> &Stat {
        self.entry.stat()
    }

    /// Opens the entry for reading when it is a regular file, through its
    /// pinned inode: a FIFO or a device node is never opened. It is judged
    /// first as a build context's file is, against the directory it was
    /// found in (ADR-0076): a hardlink planted from elsewhere, or a file the
    /// directory's owner could not read, is refused, so a checkout a
    /// lower-trust account owns outside the context cannot hand root's
    /// files to the Dockerfile reader either.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] when it is not a regular file or is refused.
    pub fn open_regular(&self) -> Result<OwnedFd> {
        self.refuse_planted(self.entry.stat(), self.path())?;
        self.entry.open_regular()
    }

    /// The context rule ([`crate::copy::context_refusal`]) for the entry
    /// `st` at `path` in this entry's directory, judged against that held
    /// directory.
    fn refuse_planted(&self, st: &Stat, path: &Path) -> Result<()> {
        let dir = stat_of(self.dir.as_fd(), path.parent().unwrap_or(path))?;
        match crate::copy::context_refusal(st, &dir) {
            Some(reason) => Err(Error::InvalidPath {
                path: path.to_owned(),
                reason,
            }),
            None => Ok(()),
        }
    }

    /// Opens the regular file `name` in the entry's directory, without
    /// following it: `None` when it is missing. It is pinned and checked
    /// first, so a symlink, a FIFO or a device node there is refused, never
    /// opened, and judged against that directory as
    /// [`HostFile::open_regular`] judges the entry.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] for anything but a regular file, or a file the
    /// context rule refuses; [`Error::Io`] when it cannot be opened.
    pub fn open_beside(&self, name: &OsStr) -> Result<Option<OwnedFd>> {
        let path = self.path().with_file_name(name);
        let pin = match root::open_name(
            self.dir.as_fd(),
            name,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            false,
            &path,
        ) {
            Ok(pin) => pin,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(None),
            Err(e) => return Err(e),
        };
        let st = stat_of(pin.as_fd(), &path)?;
        let reason = match st.file_type() {
            FileType::Regular => {
                self.refuse_planted(&st, &path)?;
                return meta::reopen(pin.as_fd(), OFlags::RDONLY, &path).map(Some);
            }
            FileType::Symlink => "is a symlink, which is not followed beside the Dockerfile",
            _ => "is not a regular file",
        };
        Err(Error::InvalidPath { path, reason })
    }
}

/// Walks the host path `path` from `/` (a relative one from the current
/// directory, made absolute first), following symlinks while the walk is
/// outside `fence`, and stopping as soon as it takes a name inside it.
///
/// A `..` straight after the fenced directory leaves it again, since the
/// directory's parent is not the context owner's. Every directory on the
/// way is opened, a symlink's target walked again name by name, so no path
/// reaches into the context without the walk seeing its root.
///
/// # Errors
///
/// [`Error::Io`] with `ENOENT` for a missing component, `ENOTDIR` for a
/// non-directory in the middle, `ELOOP` past 40 symlinks.
pub fn reach(path: &Path, fence: &Fence) -> Result<Reached> {
    let abs = std::path::absolute(path).map_err(|source| Error::Io {
        op: "make absolute",
        path: path.to_owned(),
        source,
    })?;
    let mut rest = names(&abs);
    let mut cur = open_slash()?;
    let mut at = PathBuf::from("/");
    let mut at_fence = fence.is(&stat_of(cur.as_fd(), &at)?);
    let mut links = 0;
    while let Some(name) = rest.pop_front() {
        if name == ".." {
            cur = parent_of(cur.as_fd(), &at)?;
            at.pop();
            at_fence = fence.is(&stat_of(cur.as_fd(), &at)?);
            continue;
        }
        if at_fence {
            // A name inside the context: everything from here is its owner's.
            rest.push_front(name);
            return Ok(Reached::Inside(rest.into_iter().collect()));
        }
        let here = at.join(&name);
        let fd = root::open_name(
            cur.as_fd(),
            &name,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            false,
            &here,
        )?;
        let st = stat_of(fd.as_fd(), &here)?;
        match st.file_type() {
            FileType::Symlink => {
                if !trusted_link(&st, &stat_of(cur.as_fd(), &at)?) {
                    return Err(Error::InvalidPath {
                        path: here,
                        reason: UNTRUSTED_LINK,
                    });
                }
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(io_err("resolve", &abs, Errno::LOOP));
                }
                let target = rustix::fs::readlinkat(&fd, "", Vec::new())
                    .map_err(|e| io_err("readlink", &here, e))?;
                let target = PathBuf::from(OsString::from_vec(target.into_bytes()));
                if target.is_absolute() {
                    cur = open_slash()?;
                    at = PathBuf::from("/");
                    at_fence = fence.is(&stat_of(cur.as_fd(), &at)?);
                }
                for n in names(&target).into_iter().rev() {
                    rest.push_front(n);
                }
            }
            FileType::Directory => {
                at_fence = fence.is(&st);
                cur = fd;
                at = here;
            }
            _ if rest.is_empty() => {
                return Ok(Reached::Outside(HostFile {
                    entry: Pinned::new(fd, st, here),
                    dir: cur,
                }));
            }
            _ => return Err(io_err("resolve", &here, Errno::NOTDIR)),
        }
    }
    if at_fence {
        return Ok(Reached::Inside(PathBuf::new()));
    }
    let st = stat_of(cur.as_fd(), &at)?;
    let dir = parent_of(cur.as_fd(), &at)?;
    Ok(Reached::Outside(HostFile {
        entry: Pinned::new(cur, st, at),
        dir,
    }))
}

/// A host directory an output goes in (ADR-0076): reached from `/` one name
/// at a time, never through a symlink and never into the build context, and
/// written only by descriptor.
#[derive(Debug)]
pub struct OutputDir {
    fd: OwnedFd,
    path: PathBuf,
    fence: Option<Fence>,
}

impl OutputDir {
    /// Opens the directory `path` (a relative one from the current
    /// directory, made absolute first). With `create`, missing directories
    /// are made with that mode (less the umask), each by `mkdirat` below the
    /// last one reached and opened again without following a symlink, so a
    /// name swapped in between is refused. `fence`, the build context, must
    /// not be on the way, or be the directory itself.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] for a symlink on the way or a path into the
    /// context; [`Error::Io`] for a missing directory without `create`, a
    /// non-directory on the way, or a failed `mkdirat`.
    pub fn open(path: &Path, create: Option<u32>, fence: Option<&Fence>) -> Result<Self> {
        let abs = std::path::absolute(path).map_err(|source| Error::Io {
            op: "make absolute",
            path: path.to_owned(),
            source,
        })?;
        let mut dir = Self {
            fd: open_slash()?,
            path: PathBuf::from("/"),
            fence: fence.copied(),
        };
        dir.check_fence()?;
        for c in abs.components() {
            match c {
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
                Component::ParentDir => {
                    dir.fd = parent_of(dir.fd.as_fd(), &dir.path)?;
                    dir.path.pop();
                    dir.check_fence()?;
                }
                Component::Normal(name) => dir = dir.sub_dir(name, create)?,
            }
        }
        Ok(dir)
    }

    /// Checks, before anything is written, that [`OutputDir::open`] would
    /// accept `path`: every directory of it that exists is reached without a
    /// symlink and outside `fence`. Missing ones are fine: `open` makes
    /// them.
    ///
    /// # Errors
    ///
    /// As [`OutputDir::open`], except for missing directories.
    pub fn check(path: &Path, fence: Option<&Fence>) -> Result<()> {
        match Self::open(path, None, fence) {
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// The directory's path, as walked.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory `name` in this one, made with `create` when missing
    /// (as [`OutputDir::open`]).
    ///
    /// # Errors
    ///
    /// As [`OutputDir::open`].
    pub fn sub_dir(&self, name: &OsStr, create: Option<u32>) -> Result<Self> {
        let path = self.path.join(name);
        let open = || {
            root::open_name(
                self.fd.as_fd(),
                name,
                OFlags::PATH | OFlags::DIRECTORY,
                0,
                false,
                &path,
            )
        };
        let fd = match open() {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                let Some(mode) = create else {
                    return Err(e);
                };
                match rustix::fs::mkdirat(
                    &self.fd,
                    name,
                    Mode::from_raw_mode(mode & meta::PERMISSION_BITS),
                ) {
                    // Made by someone else in between: opened like any other.
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(io_err("mkdir", &path, e)),
                }
                open().map_err(|e| refusal(e, &path))?
            }
            Err(e) => return Err(refusal(e, &path)),
        };
        let dir = Self {
            fd,
            path,
            fence: self.fence,
        };
        dir.check_fence()?;
        Ok(dir)
    }

    /// Refuses this directory when it is the build context's.
    fn check_fence(&self) -> Result<()> {
        if let Some(fence) = &self.fence
            && fence.is(&stat_of(self.fd.as_fd(), &self.path)?)
        {
            return Err(Error::InvalidPath {
                path: self.path.clone(),
                reason: IN_CONTEXT,
            });
        }
        Ok(())
    }

    /// Writes `bytes` to the file `name` in place, as kaniko writes its
    /// digest files (a file the operator mounted there, such as a
    /// Kubernetes termination log, keeps its inode): made exclusively with
    /// `mode` (less the umask) when missing, else truncated and rewritten.
    /// An existing entry is pinned without following it and checked first:
    /// a symlink is refused, and so is anything but a regular file with one
    /// link, since a hardlink would carry the write to its other names; a
    /// FIFO or a device node is never opened.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] for a symlink, a file that is not regular or
    /// has other links; [`Error::Io`] when a step fails.
    pub fn write_in_place(&self, name: &OsStr, mode: u32, bytes: &[u8]) -> Result<()> {
        let path = self.path.join(name);
        let fd = match root::open_name(
            self.fd.as_fd(),
            name,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            false,
            &path,
        ) {
            Ok(pin) => {
                let st = stat_of(pin.as_fd(), &path)?;
                let reason = match st.file_type() {
                    FileType::Regular if st.nlink > 1 => {
                        Some("has other hard links, which writing it would change too")
                    }
                    FileType::Regular => None,
                    FileType::Symlink => Some(THROUGH_SYMLINK),
                    _ => Some("is not a regular file"),
                };
                if let Some(reason) = reason {
                    return Err(Error::InvalidPath { path, reason });
                }
                meta::reopen(pin.as_fd(), OFlags::WRONLY, &path)?
            }
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => root::open_name(
                self.fd.as_fd(),
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOCTTY,
                mode,
                false,
                &path,
            )?,
            Err(e) => return Err(e),
        };
        rustix::fs::ftruncate(&fd, 0).map_err(|e| io_err("truncate", &path, e))?;
        let mut file = File::from(fd);
        std::io::Write::write_all(&mut file, bytes).map_err(|source| Error::Io {
            op: "write",
            path,
            source,
        })
    }

    /// Writes the file `name` through a temporary file in this directory,
    /// renamed over `name` once `write` succeeded and the data is synced: a
    /// reader never sees a partial file. Only a regular file at `name` is
    /// replaced (a hardlink's other names keep the old content), and only
    /// where nothing is otherwise (`RENAME_NOREPLACE`): a symlink, a device
    /// node (`/dev/null`), a FIFO, a socket or a directory is refused,
    /// before anything is written, and so is a directory on procfs, sysfs
    /// or another filesystem of kernel interfaces. The temporary file has a
    /// random name and is made exclusively (`O_CREAT | O_EXCL`) with `mode`
    /// (less the umask); it is removed on every error.
    ///
    /// # Errors
    ///
    /// `write`'s own error; [`Error::InvalidPath`] for a name that is no
    /// regular file or a kernel filesystem; [`Error::Io`] when the temporary
    /// file cannot be made, synced or renamed.
    pub fn write_atomically<T, E: From<Error>>(
        &self,
        name: &OsStr,
        mode: u32,
        write: impl FnOnce(&mut File) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.refuse_kernel_fs()?;
        // Before anything is written: a name that is no regular file is
        // refused, never replaced.
        self.replaceable(name)?;
        let (tmp_name, file) = self.temporary(name, mode)?;
        let tmp = Temporary {
            dir: self.fd.as_fd(),
            name: tmp_name,
            kept: false,
        };
        let mut file = file;
        let value = write(&mut file)?;
        let tmp_path = self.path.join(&tmp.name);
        file.sync_all().map_err(|source| Error::Io {
            op: "sync",
            path: tmp_path.clone(),
            source,
        })?;
        drop(file);
        self.rename_into(&tmp.name, name, &tmp_path)?;
        tmp.keep();
        Ok(value)
    }

    /// Renames the temporary file `tmp` to `name`: over a regular file
    /// there, else only where nothing is (`RENAME_NOREPLACE`), so a device
    /// node, a FIFO, a socket, a directory or a symlink at the name
    /// (`/dev/null`, `/dev/stdout`) is refused, never replaced. Where the
    /// filesystem refuses `RENAME_NOREPLACE` (NFS, 9p, FUSE without
    /// `rename2`: `EINVAL`), [`OutputDir::link_into`] takes over.
    fn rename_into(&self, tmp: &OsStr, name: &OsStr, tmp_path: &Path) -> Result<()> {
        for _ in 0..2 {
            if self.replaceable(name)? {
                return rustix::fs::renameat(&self.fd, tmp, &self.fd, name)
                    .map_err(|e| io_err("rename", tmp_path, e));
            }
            match rename_noreplace(self.fd.as_fd(), tmp, name) {
                // Made by someone else in between: looked at again.
                Err(Errno::EXIST) => {}
                Err(Errno::INVAL) => return self.link_into(tmp, name, tmp_path),
                r => return r.map_err(|e| io_err("rename", tmp_path, e)),
            }
        }
        Err(io_err("rename", tmp_path, Errno::EXIST))
    }

    /// [`OutputDir::rename_into`] where the filesystem refuses
    /// `RENAME_NOREPLACE`: a hard link `name` to `tmp`, which never replaces
    /// anything (`EEXIST`), then the temporary name removed. Where hard links
    /// are refused too, `name` is looked at again and `tmp` renamed over it
    /// plainly: between the look and the rename only a writer of this
    /// directory could put an entry there, and the rename replaces that name
    /// without following it.
    fn link_into(&self, tmp: &OsStr, name: &OsStr, tmp_path: &Path) -> Result<()> {
        for _ in 0..2 {
            match link_name(self.fd.as_fd(), tmp, name) {
                Ok(()) => {
                    if let Err(e) = rustix::fs::unlinkat(&self.fd, tmp, AtFlags::empty()) {
                        tracing::warn!(
                            "rnk-rootfs: the output {} is in place, but its temporary name {} \
                             could not be removed: {e}",
                            self.path.join(name).display(),
                            tmp_path.display()
                        );
                    }
                    return Ok(());
                }
                // A regular file appeared meanwhile: replaced by rename;
                // anything else is refused by `replaceable`.
                Err(Errno::EXIST) => {
                    if self.replaceable(name)? {
                        return rustix::fs::renameat(&self.fd, tmp, &self.fd, name)
                            .map_err(|e| io_err("rename", tmp_path, e));
                    }
                }
                Err(Errno::PERM | Errno::OPNOTSUPP | Errno::INVAL | Errno::MLINK) => {
                    self.replaceable(name)?;
                    return rustix::fs::renameat(&self.fd, tmp, &self.fd, name)
                        .map_err(|e| io_err("rename", tmp_path, e));
                }
                Err(e) => return Err(io_err("link", tmp_path, e)),
            }
        }
        Err(io_err("link", tmp_path, Errno::EXIST))
    }

    /// Whether `name` here is a regular file an output may replace (`true`)
    /// or nothing (`false`); anything else is refused, said as what it is.
    fn replaceable(&self, name: &OsStr) -> Result<bool> {
        let path = self.path.join(name);
        let pin = match root::open_name(
            self.fd.as_fd(),
            name,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            false,
            &path,
        ) {
            Ok(pin) => pin,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(false),
            Err(e) => return Err(e),
        };
        let reason = match stat_of(pin.as_fd(), &path)?.file_type() {
            FileType::Regular => return Ok(true),
            FileType::Symlink => THROUGH_SYMLINK,
            FileType::Directory => "is a directory, which an output file never replaces",
            FileType::CharDevice | FileType::BlockDevice => {
                "is a device node, which an output file never replaces; name a regular file"
            }
            FileType::Fifo | FileType::Socket => {
                "is a FIFO or a socket, which an output file never replaces; name a regular file"
            }
            FileType::Unknown => "is not a regular file, which an output file never replaces",
        };
        Err(Error::InvalidPath { path, reason })
    }

    /// Refuses a directory on a filesystem of kernel interfaces (`/proc`,
    /// `/sys` and the like), where a name is no file to replace.
    fn refuse_kernel_fs(&self) -> Result<()> {
        let fs = rustix::fs::fstatfs(&self.fd).map_err(|e| io_err("fstatfs", &self.path, e))?;
        let f_type: i128 = fs.f_type.into();
        if KERNEL_FS.contains(&f_type) {
            return Err(Error::InvalidPath {
                path: self.path.clone(),
                reason: "is on a filesystem of kernel interfaces (procfs, sysfs, devpts, cgroup, \
                         securityfs and the like), where an output file never goes",
            });
        }
        Ok(())
    }

    /// A new temporary file beside `name`, made exclusively.
    fn temporary(&self, name: &OsStr, mode: u32) -> Result<(OsString, File)> {
        let mut last = Errno::EXIST;
        for _ in 0..TMP_TRIES {
            let mut tmp = OsString::from(".");
            tmp.push(name);
            tmp.push(format!(".{}.ranikust-tmp", random_hex(&self.path)?));
            match root::open_name(
                self.fd.as_fd(),
                &tmp,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOCTTY,
                mode,
                false,
                &self.path.join(&tmp),
            ) {
                Ok(fd) => return Ok((tmp, File::from(fd))),
                Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {
                    last = Errno::EXIST;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io_err("create a temporary file in", &self.path, last))
    }

    /// Opens the regular file `name` here for reading, without following
    /// it: `None` when it is missing, a symlink or not a regular file. The
    /// entry is pinned and checked before it is opened, so a FIFO or a
    /// device node is never opened.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when it exists but cannot be opened.
    pub fn open_existing(&self, name: &OsStr) -> Result<Option<File>> {
        let path = self.path.join(name);
        let pin = match root::open_name(
            self.fd.as_fd(),
            name,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            false,
            &path,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(None),
            Err(e) => return Err(e),
        };
        if stat_of(pin.as_fd(), &path)?.file_type() != FileType::Regular {
            return Ok(None);
        }
        Ok(Some(File::from(meta::reopen(
            pin.as_fd(),
            OFlags::RDONLY,
            &path,
        )?)))
    }
}

/// A temporary file [`OutputDir::write_atomically`] made: removed on drop
/// unless it was renamed into place.
struct Temporary<'d> {
    dir: BorrowedFd<'d>,
    name: OsString,
    kept: bool,
}

impl Temporary<'_> {
    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if !self.kept {
            // Best effort: the error that got here is what the caller sees.
            let _ = rustix::fs::unlinkat(self.dir, &self.name, AtFlags::empty());
        }
    }
}

/// An `ELOOP` from a single-name open is a symlink in the way: said as such.
fn refusal(e: Error, path: &Path) -> Error {
    if e.raw_os_error() == Some(Errno::LOOP.raw_os_error()) {
        Error::InvalidPath {
            path: path.to_owned(),
            reason: THROUGH_SYMLINK,
        }
    } else {
        e
    }
}

/// The names of `p`, `.` dropped, `..` kept, a root `/` ignored.
fn names(p: &Path) -> VecDeque<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// `/`, `O_PATH | O_DIRECTORY`.
fn open_slash() -> Result<OwnedFd> {
    rustix::fs::open(
        "/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io_err("open", Path::new("/"), e))
}

/// The parent of the directory `dir` (`..`, which stays at `/`).
fn parent_of(dir: BorrowedFd<'_>, path: &Path) -> Result<OwnedFd> {
    rustix::fs::openat(
        dir,
        "..",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io_err("open the parent of", path, e))
}

/// `renameat2(RENAME_NOREPLACE)` of `from` to `to` in `dir`.
fn rename_noreplace(dir: BorrowedFd<'_>, from: &OsStr, to: &OsStr) -> rustix::io::Result<()> {
    #[cfg(test)]
    if fault::is(fault::NOREPLACE_REFUSED) {
        return Err(Errno::INVAL);
    }
    rustix::fs::renameat_with(dir, from, dir, to, rustix::fs::RenameFlags::NOREPLACE)
}

/// `linkat` of `from` to the new name `to` in `dir`, no symlink followed.
fn link_name(dir: BorrowedFd<'_>, from: &OsStr, to: &OsStr) -> rustix::io::Result<()> {
    #[cfg(test)]
    if fault::is(fault::LINK_REFUSED) {
        return Err(Errno::PERM);
    }
    rustix::fs::linkat(dir, from, dir, to, AtFlags::empty())
}

/// Faults the tests inject: filesystems that refuse `RENAME_NOREPLACE`, or
/// hard links too, which a tmpfs test directory does not.
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    pub(crate) const NOREPLACE_REFUSED: u8 = 1;
    pub(crate) const LINK_REFUSED: u8 = 2;

    thread_local! {
        static FAULTS: Cell<u8> = const { Cell::new(0) };
    }

    /// Sets the faults of this thread (a test runs on its own thread).
    pub(crate) fn set(faults: u8) {
        FAULTS.with(|f| f.set(faults));
    }

    pub(super) fn is(fault: u8) -> bool {
        FAULTS.with(|f| f.get() & fault != 0)
    }
}

fn stat_of(fd: BorrowedFd<'_>, path: &Path) -> Result<Stat> {
    meta::statx_at(fd, OsStr::new(""), path)
}

/// 16 random bytes as hex, for a temporary name nobody can guess.
fn random_hex(path: &Path) -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        let rest = bytes.get_mut(filled..).unwrap_or_default();
        match rustix::rand::getrandom(rest, rustix::rand::GetRandomFlags::empty()) {
            Ok(n) => filled += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(io_err("getrandom for a temporary name in", path, e)),
        }
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests;
