//! ranikust runs again from a sealed copy of its binary (ADR-0101).
//!
//! Every process ranikust forks runs its binary until it executes
//! something else: a `RUN` child until `execve`, a full-sandbox `RUN`'s init
//! for good. Run from a copy no `RUN` can write, none of them hands a `RUN`
//! a binary it could change through `/proc/<pid>/exe`, whatever the kernel
//! does about writes to a running file. The copies are the first two of
//! runc's (`CloneBinary`, `libcontainer/dmz` in runc 1.2.0), without the
//! overlay runc tries before them or the named temporary file after.
//!
//! It also tells which mount the binary ranikust started from is on
//! ([`BinaryMount`]), measured before the copy runs, since the copy cannot
//! see that file any more.

use std::convert::Infallible;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Seek};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rnk_rootfs::{FileType, MountInfo, RootPath};
use rustix::fs::{AtFlags, Mode, OFlags, SealFlags, StatVfsMountFlags, StatxFlags};
use rustix::io::Errno;

/// Set for the copy's run: it names the way and the mount of the binary
/// ranikust started from, and the copy never makes another (no loop).
const MARKER: &str = "RANIKUST_SEALED_COPY";

/// `FUSE_SUPER_MAGIC`.
const FUSE: rustix::fs::FsWord = 0x6573_5546;

/// `OVERLAYFS_SUPER_MAGIC`.
const OVERLAYFS: rustix::fs::FsWord = 0x794c_7630;

/// The mount table's type of a fuse-overlayfs mount (rootless podman's
/// container roots).
const FUSE_OVERLAYFS: &str = "fuse.fuse-overlayfs";

/// The seals of a memfd copy: no write, no change of size, no seal taken
/// off.
const SEALS: SealFlags = SealFlags::SEAL
    .union(SealFlags::SHRINK)
    .union(SealFlags::GROW)
    .union(SealFlags::WRITE);

/// A way to make the copy, in the order [`SealedCopy::ALL`] tries them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealedCopy {
    /// A copy in a sealed memfd: no write, no change of size. It takes the
    /// binary's size in memory.
    Memfd,
    /// A copy in an `O_TMPFILE` file no path reaches, run read-only. Only
    /// the kernel's `ETXTBSY` keeps it from being written, as the installed
    /// file before, but a write would not reach the installed file.
    Tmpfile,
}

impl SealedCopy {
    /// Every way, in the order they are tried.
    pub const ALL: [Self; 2] = [Self::Memfd, Self::Tmpfile];

    fn name(self) -> &'static str {
        match self {
            Self::Memfd => "memfd",
            Self::Tmpfile => "tmpfile",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|way| way.name() == name)
    }
}

impl fmt::Display for SealedCopy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which mount the binary ranikust started from is on, against the mount
/// of `/` (ADR-0101). In a container, a file on another writable mount (a
/// bind mount or a volume), or reached through one, is one a `RUN` on `/`
/// could alter, and it no longer runs while the build runs from its copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryMount {
    /// On the mount of `/`: in a container, its own root. On a root that
    /// is not an overlay (overlayfs or fuse-overlayfs), also with one link
    /// and no other writable mount reaching it.
    Root,
    /// On another mount, read-only, and no writable mount reaches it.
    ReadOnly,
    /// On another mount, writable.
    Writable,
    /// On another read-only mount, or on a root that is not an overlay,
    /// but another writable mount of its file system has a root that
    /// contains the file (`/proc/self/mountinfo`), or is the file itself (a
    /// bind of it or of one of its links, even one whose name was removed).
    Exposed,
    /// On another read-only mount, or on a root that is not an overlay,
    /// but the file has other hard links, which no mount table places.
    Linked,
    /// Cannot be told: `statx` gave a mount id of 0, as qemu-user's does
    /// for every file and the kernel for a memfd (Linux 6.8 and 7.0).
    NoMountId,
    /// Cannot be told: `/proc/self/mountinfo` does not place the file (a
    /// line it cannot parse, no line for its mount, a mount root that is
    /// not a path, a path outside that mount, a root of its own whose name
    /// was removed) or does not type a FUSE root; a writable mount of its
    /// file system is a directory whose name was removed, is covered by
    /// another mount, or cannot be reached at its mount point; the table
    /// changed while the walk ran; or the file has no name left.
    Unplaced,
    /// Not weighed: the caller asked for no walk of the writable mounts
    /// that may reach the file, since the build cannot be kaniko mode on a
    /// container's `/`, the only one that reads the answer.
    NotWeighed,
    /// Cannot be told: the error of the call that failed.
    Unknown(Errno),
}

impl BinaryMount {
    /// The binary's mount `mnt` against the mount `root` of `/`.
    fn of(mnt: u64, root: u64, read_only: bool) -> Self {
        // A zero id proves nothing: qemu-user's statx reports the field
        // filled and leaves it 0.
        if mnt == 0 || root == 0 {
            Self::NoMountId
        } else if mnt == root {
            Self::Root
        } else if read_only {
            Self::ReadOnly
        } else {
            Self::Writable
        }
    }

    /// Its word in the marker.
    fn token(self) -> String {
        match self {
            Self::Root => "root".to_owned(),
            Self::ReadOnly => "ro".to_owned(),
            Self::Writable => "rw".to_owned(),
            Self::Exposed => "exposed".to_owned(),
            Self::Linked => "linked".to_owned(),
            Self::NoMountId => "noid".to_owned(),
            Self::Unplaced => "unplaced".to_owned(),
            Self::NotWeighed => "unweighed".to_owned(),
            Self::Unknown(e) => format!("unknown:{}", e.raw_os_error()),
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token {
            "root" => Some(Self::Root),
            "ro" => Some(Self::ReadOnly),
            "rw" => Some(Self::Writable),
            "exposed" => Some(Self::Exposed),
            "linked" => Some(Self::Linked),
            "noid" => Some(Self::NoMountId),
            "unplaced" => Some(Self::Unplaced),
            "unweighed" => Some(Self::NotWeighed),
            _ => {
                // A malformed marker is not believed: the run ends unsealed, with why.
                let raw: i32 = token.strip_prefix("unknown:")?.parse().ok()?;
                // Only a number the kernel gives: rustix panics on others.
                (1..4096)
                    .contains(&raw)
                    .then(|| Self::Unknown(Errno::from_raw_os_error(raw)))
            }
        }
    }
}

/// What [`run_from_sealed_copy`] found, when it returns.
#[derive(Debug)]
pub enum Sealing {
    /// This process runs from a copy made that way, checked.
    Sealed {
        /// The way the copy was made.
        way: SealedCopy,
        /// Why each way tried before it failed, `; ` between them; empty
        /// when it was the first tried.
        passed_over: String,
        /// The mount of the binary ranikust started from, as the run
        /// before the copy found it.
        binary: BinaryMount,
    },
    /// This process runs from the binary as installed, or from something
    /// that is not the copy its marker names.
    Unsealed {
        /// Why.
        why: String,
        /// The mount of the binary this process runs from.
        binary: BinaryMount,
    },
}

impl Sealing {
    /// The mount of the binary ranikust started from.
    #[must_use]
    pub fn binary(&self) -> BinaryMount {
        match self {
            Self::Sealed { binary, .. } | Self::Unsealed { binary, .. } => *binary,
        }
    }
}

/// Runs this process again from a sealed copy of its binary, trying `ways`
/// in order; returns in the copy's run, or when no way worked.
///
/// Call it while the process is single-threaded, before the build's work
/// starts: the copy starts over with the same arguments, environment and
/// descriptors, `RANIKUST_SEALED_COPY` added. In the copy's run (that
/// variable set) it makes no copy, checks `/proc/self/exe` is the copy the
/// variable names, and names the process after its `argv[0]` again. The
/// core-file limit [`forbid_core_dumps`](crate::forbid_core_dumps) recorded
/// is given back for the copy, which records it again. Before any copy,
/// [`BinaryMount`] is measured on the binary as installed and passed to
/// the copy in the variable; a run that ends unsealed measures the file it
/// runs from. `walk` is whether the build may be kaniko mode on a
/// container's `/`, the only one that reads whether a writable mount
/// reaches the binary: otherwise the mount points are not looked up, and a
/// binary that needs them is [`BinaryMount::NotWeighed`].
///
/// # Errors
///
/// Only when a copy did not run and the core-file limit could not be set
/// back to 0: the process must stop, as when `forbid_core_dumps` fails.
pub fn run_from_sealed_copy(ways: &[SealedCopy], walk: bool) -> rustix::io::Result<Sealing> {
    if let Some(value) = std::env::var_os(MARKER) {
        return Ok(copy_run(&value, walk));
    }
    let exe = match File::open("/proc/self/exe") {
        Ok(exe) => exe,
        Err(e) => {
            return Ok(Sealing::Unsealed {
                why: format!("open /proc/self/exe: {e}"),
                binary: BinaryMount::Unknown(errno_of(&e)),
            });
        }
    };
    let binary = mount_of(exe.as_fd(), walk);
    let run = |way, copy: OwnedFd, failed: &[(SealedCopy, io::Error)]| {
        execute(&copy, &marker(way, binary, failed))
    };
    let make = |way| make_copy(way, &exe);
    let failed = match try_in_order(ways, make, run, core_limit_to_zero) {
        Search::Ran(never) => match never {},
        Search::NoneRan(failed) => failed,
        Search::Stopped(e) => return Err(e),
    };
    let why = if failed.is_empty() {
        "no way was tried".to_owned()
    } else {
        passed_over(&failed)
    };
    Ok(Sealing::Unsealed { why, binary })
}

/// `statx` of `path` at `fd`, with the mount id, the link count and the
/// inode.
fn statx_of(
    fd: BorrowedFd<'_>,
    path: &std::ffi::CStr,
    flags: AtFlags,
) -> Result<rustix::fs::Statx, Errno> {
    let want = StatxFlags::MNT_ID | StatxFlags::NLINK | StatxFlags::INO;
    let st = rustix::fs::statx(fd, path, flags, want)?;
    // Linux 5.8, which ranikust needs (ADR-0028), always fills it.
    if StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        Ok(st)
    } else {
        Err(Errno::NOSYS)
    }
}

/// Which mount the file `fd` is on, against the mount of `/`, and its
/// `statx`.
fn placed(fd: BorrowedFd<'_>) -> Result<(BinaryMount, rustix::fs::Statx), Errno> {
    let st = statx_of(fd, c"", AtFlags::EMPTY_PATH)?;
    let root = statx_of(rustix::fs::CWD, c"/", AtFlags::empty())?.stx_mnt_id;
    let flags = rustix::fs::fstatvfs(fd)?.f_flag;
    let mount = BinaryMount::of(
        st.stx_mnt_id,
        root,
        flags.contains(StatVfsMountFlags::RDONLY),
    );
    Ok((mount, st))
}

/// Which mount the file `exe` is on, against the mount of `/`, and, on
/// another read-only mount or a root that is not an overlay, whether
/// another writable mount reaches the file ([`verdict`]); that walk only
/// when `walk`.
fn mount_of(exe: BorrowedFd<'_>, walk: bool) -> BinaryMount {
    let found = (|| {
        let (mount, st) = placed(exe)?;
        let copies = || {
            copies_up(rustix::fs::fstatfs(exe)?.f_type, || {
                let text = std::fs::read("/proc/self/mountinfo").map_err(|e| errno_of(&e))?;
                Ok(root_type(&text, st.stx_mnt_id))
            })
        };
        let reach = || -> Result<Option<bool>, Errno> {
            // The path `/proc/self/exe` gives, named through the descriptor
            // measured above.
            let link = format!("/proc/self/fd/{}", exe.as_raw_fd());
            let path = PathBuf::from(OsString::from_vec(
                rustix::fs::readlinkat(rustix::fs::CWD, link.as_str(), Vec::new())?.into_bytes(),
            ));
            let Some(binary) = FileId::of(&st) else {
                return Ok(None);
            };
            on_a_stable_table(
                || std::fs::read("/proc/self/mountinfo").map_err(|e| errno_of(&e)),
                |mounts| reached_from_slash(mounts, st.stx_mnt_id, &path, binary),
            )
        };
        verdict(mount, copies, st.stx_nlink, walk.then_some(reach))
    })();
    found.unwrap_or_else(BinaryMount::Unknown)
}

/// Whether a write to a file in the root, on a file system of type
/// `f_type` (`statfs`), copies the file up and leaves the image's own files
/// as they are: on overlayfs, and on FUSE when the root's type in the mount
/// table (`own_type`, called only for FUSE) is fuse-overlayfs. `None` for
/// FUSE when the table gives the root no type.
fn copies_up(
    f_type: rustix::fs::FsWord,
    own_type: impl FnOnce() -> Result<Option<String>, Errno>,
) -> Result<Option<bool>, Errno> {
    Ok(match f_type {
        OVERLAYFS => Some(true),
        // FUSE serves many file systems: only the mount table names this one.
        FUSE => own_type()?.map(|t| t == FUSE_OVERLAYFS),
        _ => Some(false),
    })
}

/// The type the mount table `text` gives the mount `own`, if it parses and
/// lists it.
fn root_type(text: &[u8], own: u64) -> Option<String> {
    // A table that does not parse types nothing: the root is unplaced.
    let mounts = rnk_rootfs::parse_mountinfo(text).ok()?;
    mounts
        .into_iter()
        .find(|m| m.mount_id == own)
        .map(|m| m.fs_type)
}

/// The binary's mount once its links and the mounts that reach it are
/// weighed: on another read-only mount, and on a root where a write does
/// not copy up (`copies`, [`copies_up`]), a file with no link or one the
/// mount table does not place (`reach` gives `None`) is
/// [`BinaryMount::Unplaced`], one with other links [`BinaryMount::Linked`],
/// one another writable mount reaches [`BinaryMount::Exposed`]. A file on
/// a root the table does not type (`copies` gives `None`) is `Unplaced`,
/// before its links are weighed. `copies` and `reach` are called only when
/// needed; their error is returned, for [`BinaryMount::Unknown`]. With no
/// `reach`, a file that needs it is [`BinaryMount::NotWeighed`].
fn verdict(
    mount: BinaryMount,
    copies: impl FnOnce() -> Result<Option<bool>, Errno>,
    nlink: u32,
    reach: Option<impl FnOnce() -> Result<Option<bool>, Errno>>,
) -> Result<BinaryMount, Errno> {
    let weighed = match mount {
        BinaryMount::ReadOnly => true,
        // On an overlay root a write copies the file up and leaves the
        // layers' own files, an image's links among them, as they are.
        BinaryMount::Root => match copies()? {
            Some(up) => !up,
            None => return Ok(BinaryMount::Unplaced),
        },
        BinaryMount::Writable
        | BinaryMount::Exposed
        | BinaryMount::Linked
        | BinaryMount::NoMountId
        | BinaryMount::Unplaced
        | BinaryMount::NotWeighed
        | BinaryMount::Unknown(_) => false,
    };
    if !weighed {
        return Ok(mount);
    }
    match nlink {
        // No name: readlink cannot place it.
        0 => return Ok(BinaryMount::Unplaced),
        1 => {}
        _ => return Ok(BinaryMount::Linked),
    }
    let Some(reach) = reach else {
        return Ok(BinaryMount::NotWeighed);
    };
    Ok(match reach()? {
        None => BinaryMount::Unplaced,
        Some(true) => BinaryMount::Exposed,
        Some(false) => mount,
    })
}

/// A file's identity: the device `statx` gives it and its inode number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    dev_major: u32,
    dev_minor: u32,
    ino: u64,
}

impl FileId {
    /// The identity in `st`, if it holds the inode number.
    fn of(st: &rustix::fs::Statx) -> Option<Self> {
        StatxFlags::from_bits_retain(st.stx_mask)
            .contains(StatxFlags::INO)
            .then_some(Self {
                dev_major: st.stx_dev_major,
                dev_minor: st.stx_dev_minor,
                ino: st.stx_ino,
            })
    }
}

/// What a path leads to: a directory, or a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MountRoot {
    /// A directory: its path in the file system tells what it holds.
    Directory,
    /// Anything else: that one file, whatever its name.
    File(FileId),
}

/// What a physical path leads to: the mount, whether the path is that
/// mount's root, and the entry found there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    mount: u64,
    mount_root: bool,
    found: MountRoot,
}

/// What `path` leads to from `root` through `RootPath` (`statx`, no open;
/// the paths lead through the container's own files). The path is one the
/// kernel printed, with no symlink in it: one on the way means the tree
/// changed since, and places nothing. `None` when the path leads nowhere;
/// `Err` when the call fails otherwise (`EIO` for an error with no errno).
fn seen_at(root: &RootPath, path: &Path) -> Result<Option<Seen>, Errno> {
    let st = match root.statx_physical(path) {
        Ok(st) => st,
        Err(e) => {
            return match e.io_error().map(errno_of) {
                // A name that leads nowhere places nothing: the caller fails closed.
                Some(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => Ok(None),
                Some(errno) => Err(errno),
                // A path RootPath refuses with no errno is not a missing name.
                None => Err(Errno::IO),
            };
        }
    };
    let found = match st.file_type() {
        FileType::Directory => MountRoot::Directory,
        FileType::Regular
        | FileType::Symlink
        | FileType::CharDevice
        | FileType::BlockDevice
        | FileType::Fifo
        | FileType::Socket
        | FileType::Unknown => MountRoot::File(FileId {
            dev_major: st.dev_major,
            dev_minor: st.dev_minor,
            ino: st.ino,
        }),
    };
    Ok(Some(Seen {
        mount: st.mnt_id,
        mount_root: st.mount_root,
        found,
    }))
}

impl Seen {
    /// What was found at `m`'s mount point, if it is the root of `m`: not
    /// another mount (one mounted over it), nor an entry inside `m`.
    fn root_of(self, m: &MountInfo) -> Option<MountRoot> {
        (self.mount == m.mount_id && self.mount_root).then_some(self.found)
    }
}

/// Whether another mount of `mounts` hides `m`: one at its mount point or
/// above it that is not one of the mounts `m` lies on (its parents), so
/// mounted over it later. Told from the table alone, before any lookup.
fn covered(mounts: &[MountInfo], m: &MountInfo) -> bool {
    let mut below = Vec::new();
    let mut parent = m.parent_id;
    // A cycle cannot be longer than the table.
    for _ in 0..mounts.len() {
        let Some(p) = mounts.iter().find(|p| p.mount_id == parent) else {
            break;
        };
        below.push(p.mount_id);
        parent = p.parent_id;
    }
    mounts.iter().any(|o| {
        o.mount_id != m.mount_id
            && !below.contains(&o.mount_id)
            && m.mount_point.starts_with(&o.mount_point)
    })
}

/// [`reached_writable`] with the binary's name and each mount point looked
/// up from this process's `/` ([`seen_at`]), the binary's name not looked
/// up when its own mount is [`covered`] or absent from the table; `Err`
/// when `/` cannot be opened, or when a lookup fails and no writable mount
/// was found to reach the binary.
fn reached_from_slash(
    mounts: &[MountInfo],
    own: u64,
    path: &Path,
    binary: FileId,
) -> Result<Option<bool>, Errno> {
    let root = RootPath::open("/").map_err(|e| e.io_error().map_or(Errno::IO, errno_of))?;
    reached_by(mounts, own, path, binary, |p| seen_at(&root, p))
}

/// [`reached_from_slash`] with each path looked up by `look` ([`seen_at`]).
fn reached_by(
    mounts: &[MountInfo],
    own: u64,
    path: &Path,
    binary: FileId,
    mut look: impl FnMut(&Path) -> Result<Option<Seen>, Errno>,
) -> Result<Option<bool>, Errno> {
    // `readlink` names a removed file `<path> (deleted)`: only a name that
    // still leads to the binary, on its own mount, places it.
    let mut failed = None;
    // A name on a hidden mount leads elsewhere: it is not looked up.
    let hidden = mounts
        .iter()
        .find(|m| m.mount_id == own)
        .is_none_or(|m| covered(mounts, m));
    let named = !hidden
        && match look(path) {
            Ok(seen) => seen.is_some_and(|s| s.mount == own && s.found == MountRoot::File(binary)),
            Err(errno) => {
                failed = Some(errno);
                false
            }
        };
    let reach = reached_writable(mounts, own, named.then_some(path), binary, |m| {
        match look(&m.mount_point) {
            Ok(seen) => seen.and_then(|s| s.root_of(m)),
            Err(errno) => {
                failed.get_or_insert(errno);
                None
            }
        }
    });
    match (reach, failed) {
        (Some(true), _) | (_, None) => Ok(reach),
        (_, Some(errno)) => Err(errno),
    }
}

/// `walk` on the mount table `read` gives, read again once it is done:
/// mount ids are reused, so a table that changed meanwhile places nothing
/// (`None`), whatever the walk found, and nor does one that does not
/// parse. A failed read is its error, the second one too, even after the
/// walk found a reach: both refuse.
fn on_a_stable_table(
    mut read: impl FnMut() -> Result<Vec<u8>, Errno>,
    walk: impl FnOnce(&[MountInfo]) -> Result<Option<bool>, Errno>,
) -> Result<Option<bool>, Errno> {
    let text = read()?;
    // The marker has room for a token only; the refusal lists the possible causes.
    let Ok(mounts) = rnk_rootfs::parse_mountinfo(&text) else {
        return Ok(None);
    };
    let reach = walk(&mounts);
    if read()? != text {
        return Ok(None);
    }
    reach
}

/// Whether another writable mount of the file system of the mount `own`
/// reaches the file `binary`: a root that contains the file's path in its
/// file system (`own`'s root joined with the file's `path`, as this
/// process's root names it, below `own`'s mount point), or a root that is
/// the file itself, as `root_of` tells (a bind of a link of it, even one
/// whose name was removed). `own` itself does not count (a writable root
/// is the container's). `path` is `None` when the file's name no longer
/// leads to it. `None` when `mounts` does not place the file (no name, or
/// its own root's name removed, and no root that is the file), or when a
/// mount that may reach it is a directory whose name was removed or whose
/// root is no path, one another mount covers ([`covered`], not passed to
/// `root_of`), or one `root_of` cannot reach.
fn reached_writable(
    mounts: &[MountInfo],
    own: u64,
    path: Option<&Path>,
    binary: FileId,
    mut root_of: impl FnMut(&MountInfo) -> Option<MountRoot>,
) -> Option<bool> {
    let mine = mounts.iter().find(|m| m.mount_id == own)?;
    if !mine.fs_root.is_absolute() {
        return None;
    }
    // A removed name tells nothing of where the file's own name is now.
    let in_fs = path
        .filter(|_| !mine.root_deleted)
        .and_then(|path| path.strip_prefix(&mine.mount_point).ok())
        .map(|below| mine.fs_root.join(below));
    let writable = || {
        mounts.iter().filter(|m| {
            m.mount_id != own
                // mountinfo's device on both sides: `st_dev` differs on btrfs subvolumes.
                && m.device == mine.device
                && !m.read_only
        })
    };
    // By path first: a tree that holds the file needs no mount point reached.
    if let Some(in_fs) = &in_fs
        && writable().any(|m| !m.root_deleted && in_fs.starts_with(&m.fs_root))
    {
        return Some(true);
    }
    let mut placed = in_fs.is_some();
    for m in writable() {
        // A hidden mount is not looked up: its mount point leads elsewhere.
        let root = if covered(mounts, m) { None } else { root_of(m) };
        match root {
            // A file bind writes that one inode only: RUN cannot mount (ADR-0078).
            Some(MountRoot::File(id)) => {
                if id == binary {
                    return Some(true);
                }
            }
            // A directory whose name was removed, or that is no path, cannot
            // be told apart from a tree that holds the file.
            Some(MountRoot::Directory) => placed &= !m.root_deleted && m.fs_root.is_absolute(),
            None => placed = false,
        }
    }
    placed.then_some(false)
}

// `EIO` stands in for an `io::Error` that carries no errno.
fn errno_of(e: &io::Error) -> Errno {
    Errno::from_io_error(e).unwrap_or(Errno::IO)
}

/// Each failure with its way, `; ` between them.
fn passed_over(failed: &[(SealedCopy, io::Error)]) -> String {
    let why: Vec<String> = failed
        .iter()
        .map(|(way, e)| format!("{way}: {e}"))
        .collect();
    why.join("; ")
}

/// The marker's value for a copy made `way`: its name and the binary's
/// mount, then why the ways before it failed, for the copy to log.
fn marker(way: SealedCopy, binary: BinaryMount, failed: &[(SealedCopy, io::Error)]) -> String {
    let head = format!("{way} {}", binary.token());
    if failed.is_empty() {
        head
    } else {
        format!("{head}; {}", passed_over(failed))
    }
}

/// The way and the binary's mount a marker names, and why the ways before
/// it failed.
fn parse_marker(value: &str) -> Option<(SealedCopy, BinaryMount, &str)> {
    let (head, passed_over) = value.split_once(';').unwrap_or((value, ""));
    let (name, binary) = head.split_once(' ')?;
    Some((
        SealedCopy::from_name(name)?,
        BinaryMount::from_token(binary)?,
        passed_over.trim_start(),
    ))
}

/// The copy's run: checks the copy, never makes another.
fn copy_run(value: &OsStr, walk: bool) -> Sealing {
    // The kernel named the process after the descriptor, not `argv[0]`;
    // `ps`, `pgrep` and `pkill -x` look for ranikust's own name.
    name_after_argv0();
    let exe = match File::open("/proc/self/exe") {
        Ok(exe) => exe,
        Err(e) => {
            return Sealing::Unsealed {
                why: format!("{MARKER} is set, but /proc/self/exe does not open: {e}"),
                binary: BinaryMount::Unknown(errno_of(&e)),
            };
        }
    };
    // No checked copy: the marker is not believed, and the binary measured
    // is the one this process runs from.
    let unsealed = |why| Sealing::Unsealed {
        why,
        binary: mount_of(exe.as_fd(), walk),
    };
    let Some((way, binary, passed_over)) = value.to_str().and_then(parse_marker) else {
        return unsealed(format!(
            "{MARKER}={value:?} names no way to seal the binary"
        ));
    };
    match check(way, exe.as_fd()) {
        Ok(()) => Sealing::Sealed {
            way,
            passed_over: passed_over.to_owned(),
            binary,
        },
        Err(e) => unsealed(format!(
            "{MARKER}={way}, but /proc/self/exe is no such copy: {e}"
        )),
    }
}

fn name_after_argv0() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let base = Path::new(&argv0)
        .file_name()
        .map_or(b"ranikust".as_slice(), OsStrExt::as_bytes);
    if let Ok(name) = CString::new(base) {
        // Refused only for a bad name; the name is cosmetic.
        let _ = rustix::thread::set_name(&name);
    }
}

/// Whether `file` is a copy made `way`.
fn check(way: SealedCopy, file: BorrowedFd<'_>) -> io::Result<()> {
    let holds = match way {
        SealedCopy::Memfd => rustix::fs::fcntl_get_seals(file)?.contains(SEALS),
        SealedCopy::Tmpfile => rustix::fs::fstat(file)?.st_nlink == 0,
    };
    if holds {
        Ok(())
    } else {
        Err(io::Error::other(format!("not a sealed {way} copy")))
    }
}

/// How [`try_in_order`] ended.
#[derive(Debug)]
enum Search<T, E> {
    /// A copy ran (in the process, it never returns).
    Ran(T),
    /// No way ran: each failure with its way, in order.
    NoneRan(Vec<(SealedCopy, io::Error)>),
    /// A copy did not run and `recover` failed: no other way was tried.
    Stopped(E),
}

/// Makes a copy with each way in turn and runs it, giving `run` the
/// failures of the ways before, until one runs. After a copy that did not
/// run, `recover` must succeed for the next way to be tried.
fn try_in_order<T, E>(
    ways: &[SealedCopy],
    mut make: impl FnMut(SealedCopy) -> io::Result<OwnedFd>,
    mut run: impl FnMut(SealedCopy, OwnedFd, &[(SealedCopy, io::Error)]) -> io::Result<T>,
    mut recover: impl FnMut() -> Result<(), E>,
) -> Search<T, E> {
    let mut failed = Vec::new();
    for &way in ways {
        let copy = match make(way) {
            Ok(copy) => copy,
            Err(e) => {
                failed.push((way, e));
                continue;
            }
        };
        match run(way, copy, &failed) {
            Ok(done) => return Search::Ran(done),
            Err(e) => failed.push((way, e)),
        }
        if let Err(e) = recover() {
            return Search::Stopped(e);
        }
    }
    Search::NoneRan(failed)
}

fn make_copy(way: SealedCopy, exe: &File) -> io::Result<OwnedFd> {
    match way {
        SealedCopy::Memfd => memfd_copy(exe),
        SealedCopy::Tmpfile => tmpfile_copy(exe, &temp_dir()),
    }
}

/// Where the `O_TMPFILE` copy goes.
fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir();
    if dir.is_absolute() {
        dir
    } else {
        PathBuf::from("/tmp")
    }
}

/// A copy in a memfd sealed against writes and size changes.
fn memfd_copy(exe: &File) -> io::Result<OwnedFd> {
    use rustix::fs::MemfdFlags;
    let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING;
    // `MFD_EXEC` (Linux 6.3) keeps `vm.memfd_noexec=1` from making the copy
    // impossible to run; older kernels refuse the flag. At 2 the kernel
    // refuses it, and the next way is tried.
    let fd = match rustix::fs::memfd_create("ranikust", flags | MemfdFlags::EXEC) {
        Err(Errno::INVAL) => rustix::fs::memfd_create("ranikust", flags),
        made => made,
    }
    .map_err(context("memfd_create"))?;
    let mut copy = File::from(fd);
    copy_whole(exe, &mut copy)?;
    rustix::fs::fcntl_add_seals(&copy, SEALS).map_err(context("seal the memfd"))?;
    Ok(copy.into())
}

/// Whether a file system of type `f_type` (`statfs`) is FUSE: its daemon
/// could hand the kernel other pages to run than the ones written.
fn serves_its_own_pages(f_type: rustix::fs::FsWord) -> bool {
    f_type == FUSE
}

/// A copy in an `O_TMPFILE` file in `dir`, which can never get a name
/// (`O_EXCL`), open read-only: `execve` refuses a file open for writing.
/// Never made directly on FUSE: `dir`'s own file system type is read.
fn tmpfile_copy(exe: &File, dir: &Path) -> io::Result<OwnedFd> {
    let at = rustix::fs::open(
        dir,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(context(format!("open {}", dir.display())))?;
    let st = rustix::fs::fstatfs(&at).map_err(context(format!("statfs {}", dir.display())))?;
    if serves_its_own_pages(st.f_type) {
        return Err(io::Error::other(format!(
            "{} is on FUSE, whose daemon serves the copy's pages",
            dir.display()
        )));
    }
    let fd = rustix::fs::openat(
        &at,
        c".",
        OFlags::TMPFILE | OFlags::RDWR | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RWXU,
    )
    .map_err(context(format!("O_TMPFILE in {}", dir.display())))?;
    let mut writer = File::from(fd);
    copy_whole(exe, &mut writer)?;
    let copy = rustix::fs::open(
        format!("/proc/self/fd/{}", writer.as_raw_fd()).as_str(),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(context("open the copy read-only"))?;
    drop(writer);
    Ok(copy)
}

fn copy_whole(exe: &File, to: &mut File) -> io::Result<()> {
    let size = exe.metadata()?.len();
    let mut from = exe;
    // A way tried before read it already.
    from.rewind().map_err(context("rewind the binary"))?;
    let copied = io::copy(&mut from, to).map_err(context("copy the binary"))?;
    if copied == size {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "copied {copied} bytes of the binary's {size}"
        )))
    }
}

/// Runs `copy` with this process's arguments and environment, the marker
/// set to `marker`. Returns only when the kernel refused it.
fn execute(copy: &OwnedFd, marker: &str) -> io::Result<Infallible> {
    let args = std::env::args_os()
        .map(|arg| CString::new(arg.into_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    let env = copy_environment(std::env::vars_os(), marker)?;
    let argv = null_terminated(&args);
    let envp = null_terminated(&env);
    // The copy records the limit `forbid_core_dumps` found, as this run
    // did, so every `RUN` still gets the original back.
    if let Some(limit) = crate::spec::original_core_limit() {
        rustix::process::setrlimit(rustix::process::Resource::Core, limit)
            .map_err(context("setrlimit(RLIMIT_CORE) for the copy"))?;
    }
    // Not `Command::exec`: on a refusal it would leave `SIGPIPE` at its
    // default and the signal mask cleared in this process, which goes on.
    // The descriptor itself, close-on-exec, not `/proc/self/fd/<n>`: the
    // kernel refuses to hand a file the exec closes to an interpreter
    // (binfmt_misc, which runs an emulator, or a `#!` line) before the
    // point of no return, and the next way is tried.
    // SAFETY: the empty path, every string and both null-terminated arrays
    // outlive the call; the integers are passed as `long`, the width
    // `syscall` reads its variadic arguments at; `execveat` returns only on
    // failure, changing nothing.
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            libc::c_long::from(copy.as_raw_fd()),
            c"".as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            libc::c_long::from(libc::AT_EMPTY_PATH),
        )
    };
    Err(io::Error::last_os_error())
}

/// The copy's environment: `vars` with `MARKER=marker` in place of any
/// marker among them.
fn copy_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    marker: &str,
) -> io::Result<Vec<CString>> {
    let mut env = vars
        .into_iter()
        .filter(|(key, _)| key != MARKER)
        .map(|(key, value)| {
            let mut pair = key.into_vec();
            pair.push(b'=');
            pair.extend(value.into_vec());
            CString::new(pair)
        })
        .collect::<Result<Vec<_>, _>>()?;
    env.push(CString::new(format!("{MARKER}={marker}"))?);
    Ok(env)
}

fn null_terminated(strings: &[CString]) -> Vec<*const libc::c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// After a copy did not run: the soft core-file limit back to 0.
fn core_limit_to_zero() -> rustix::io::Result<()> {
    if crate::spec::original_core_limit().is_none() {
        return Ok(());
    }
    crate::forbid_core_dumps()
}

fn context<E: Into<io::Error>>(what: impl fmt::Display) -> impl FnOnce(E) -> io::Error {
    move |e| {
        let e = e.into();
        io::Error::new(e.kind(), format!("{what}: {e}"))
    }
}

#[cfg(test)]
mod tests;
