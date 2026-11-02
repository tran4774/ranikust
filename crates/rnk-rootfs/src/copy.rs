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
use crate::mode::Chmod;
use crate::path;
use crate::root::{self, Mounts, RootPath};
use crate::user::Owner;

/// Mode of a directory until its own mode is set at the end.
const DIR_PENDING_MODE: u32 = 0o700;
/// How [`copy`] sets ownership and modes; the builder maps `--chown`,
/// `--chmod` and the kind of source onto it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyOpts {
    /// Owner of every copied entry and of the directories the copy creates.
    /// `None` keeps each source entry's owner, as `COPY --from` without
    /// `--chown` does; the parents it creates then keep the process's
    /// owner. Context sources without `--chown` pass `0:0`, Docker's rule.
    pub owner: Option<Owner>,
    /// `--chmod` for every copied entry but symlinks, directories included,
    /// as BuildKit applies it (Q-120): an octal mode replaces the bits, and
    /// its permission bits are the mode of the directories the copy
    /// creates; a symbolic one
    /// changes each entry's own bits. `None` keeps the raw bits of the
    /// source, and created directories get `0755`.
    pub chmod: Option<Chmod>,
    /// Leaves device nodes, FIFOs and sockets out instead of making them
    /// again (`mknodat`, or an empty file for a socket), as kaniko mode's
    /// extraction and exporter leave them out (ADR-0041): kaniko mode
    /// copying one stage directory into another (ADR-0059), and `COPY`
    /// into a root a `RUN` enters without `nodev`, `Isolation::Direct`
    /// (ADR-0060). What is left out is warned about once per copy.
    pub skip_special: bool,
    /// A copy out of the build context (ADR-0076): each regular file is
    /// checked against the directory it was found in ([`context_refusal`]),
    /// since ranikust reads as root and the copy would otherwise carry a
    /// file the directory's owner cannot read into the image. `false` for a
    /// copy between roots (`COPY --from`, stage directories).
    pub from_context: bool,
}

/// Why a file is refused by the context rule: a hardlink planted from
/// elsewhere. Said of the file, so it fits a `COPY` source, the Dockerfile
/// and an ignore file alike.
const PLANTED_LINK: &str = "is a hardlink owned by another account than the directory it is \
     in, as one made to a file from elsewhere is; it is not read (ADR-0076)";

/// Why a file is refused by the context rule: its directory's owner cannot
/// read it.
const UNREADABLE: &str = "is a file the owner of the directory it is in could not read by its \
     mode bits (a root-only file linked in from elsewhere); it is not read (ADR-0076)";

/// Why a file is refused in a directory root owns that others may write:
/// a hardlink any of them could have planted.
const SHARED_LINK: &str = "is a hardlink in a directory root owns that its group or others may \
     write, where another account could have planted it; it is not read (ADR-0076)";

/// Why a file is refused in a directory root owns that others may write:
/// one others could not read, which any of them could have linked in.
const SHARED_UNREADABLE: &str = "is a file others could not read by its mode bits, in a \
     directory root owns that its group or others may write, where another account could have \
     linked a root-only file in; it is not read (ADR-0076)";

/// Whether the regular file `st`, found in the directory `dir` of a build
/// context, is refused, and why (ADR-0076). It is refused when it has
/// other links and another owner than the directory's (a hardlink planted
/// from outside, which `fs.protected_hardlinks=0` allows), or when the
/// directory's owner could not read it by its mode bits: owner bits when
/// the owner is the file's, group bits when the directory's group is the
/// file's, else the bits for others. A directory root owns reads anything,
/// as root does, unless its group or others may write it: then any of them
/// could have planted the file, so it must have one link and be readable
/// by others. Same-owner hardlinks (a pnpm store) pass elsewhere.
///
/// The rule is an approximation of what the directory's owner may read:
/// it does not look the owner's supplementary groups up, nor read ACLs, so
/// it fails closed in the usual direction only (a file readable through
/// another of the owner's groups, or an ACL grant, is refused), and can
/// pass a file a deny entry or a group-excluding mode would deny.
pub(crate) fn context_refusal(st: &Stat, dir: &Stat) -> Option<&'static str> {
    if st.file_type() != FileType::Regular {
        return None;
    }
    if st.nlink > 1 && st.uid != dir.uid {
        return Some(PLANTED_LINK);
    }
    if dir.uid == 0 && dir.mode & 0o022 != 0 {
        if st.nlink > 1 {
            return Some(SHARED_LINK);
        }
        return (st.mode & 0o004 == 0).then_some(SHARED_UNREADABLE);
    }
    let readable = dir.uid == 0
        || if st.uid == dir.uid {
            st.mode & 0o400 != 0
        } else if st.gid == dir.gid {
            st.mode & 0o040 != 0
        } else {
            st.mode & 0o004 != 0
        };
    (!readable).then_some(UNREADABLE)
}

/// Whether `t` is a node [`CopyOpts::skip_special`] leaves out.
fn special(t: FileType) -> bool {
    matches!(
        t,
        FileType::Fifo | FileType::CharDevice | FileType::BlockDevice | FileType::Socket
    )
}

/// Copies `from` in the root `src` to `to` in the root `dst`.
///
/// - `from` is resolved inside `src`, following symlinks there, the last
///   one included, as BuildKit (Q-120); `""` or `/` names the root of
///   `src`.
/// - A directory has its contents copied into the directory `to`: `to`
///   and its missing parents are created (an octal `--chmod`'s permission
///   bits, else `0755`; owner [`CopyOpts::owner`], or the process's when
///   that is `None`, as BuildKit's `MkdirAll`; `to` gets the source
///   directory's times). An existing `to` is merged into and
///   keeps its metadata; a symlink there is followed inside `dst`, a
///   non-directory replaced.
/// - Anything else is copied to exactly `to`, whose missing parents are
///   created; `to` is the file's own name, which the builder derives from
///   the destination the Dockerfile gives. An existing non-directory there
///   is replaced, never followed; an existing directory is an error.
///
/// Inside a copied directory, symlinks are copied as they are, never
/// followed; hardlinks between copied entries are kept; FIFOs and device
/// nodes are recreated, never opened (a device node needs `CAP_MKNOD`); a
/// socket becomes an empty regular file with its mode, as BuildKit's
/// `copyDevice` makes it (`fsutil copy_nowindows.go:33-45`). Each entry
/// gets the source's owner or [`CopyOpts::owner`], raw mode or
/// [`CopyOpts::chmod`], modification time (also as access time) and its
/// xattrs, as BuildKit copies them all (Q-120) except overlay's own; an
/// xattr the destination refuses is left out with a warning, as BuildKit
/// only logs it. Directories over
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
    copy_filtered(src, from, dst, to, o, &|_, _| true)
}

/// [`copy`], leaving out the entries below `from` that `keep` refuses.
///
/// `keep` gets each entry's path inside `src` (`from` joined with the
/// names below it) and whether it is a directory; a refused directory is
/// skipped with everything below it. `from` itself is always copied. The
/// builder passes the `.dockerignore` result of the context walk, so an
/// ignored file is never read.
///
/// An entry that cannot be opened without leaving its mount (a mount point
/// below `from`) is offered to `keep` as a directory, before its type is
/// known: refused, it is skipped (kaniko mode copying its root out, mounts
/// on its ignore list); kept, the copy fails as [`copy`] does.
///
/// # Errors
///
/// As [`copy`].
pub fn copy_filtered(
    src: &RootPath,
    from: impl AsRef<Path>,
    dst: &RootPath,
    to: impl AsRef<Path>,
    o: &CopyOpts,
    keep: &dyn Fn(&Path, bool) -> bool,
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
        o: o.clone(),
        keep,
        dst_dirs: HashSet::new(),
        links: HashMap::new(),
        stats: Stats::default(),
        skipped: meta::SkippedXattrs::default(),
        special: SkippedSpecial::default(),
    };
    let r = c.run(src, from.as_ref(), to.as_ref());
    c.skipped.warn();
    c.special.warn(from.as_ref());
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

/// The longest path below a root a path-based operation reaches
/// (`PATH_MAX` less the NUL), as `rnk-snapshot`'s exporter and walk use.
const PATH_MAX: usize = 4095;

/// Why an entry past [`PATH_MAX`] is refused, in the words `rnk-snapshot`
/// uses.
const PATH_TOO_LONG: &str = "an entry's path is longer than PATH_MAX";

struct Copier<'r> {
    dst: &'r RootPath,
    o: CopyOpts,
    /// Which entries below the source to copy ([`copy_filtered`]).
    keep: &'r dyn Fn(&Path, bool) -> bool,
    /// Every destination directory this copy created or entered. A source
    /// directory among them is the destination inside the source (`COPY .
    /// /workspace/sub` on a live root), which would recurse without end.
    dst_dirs: HashSet<Inode>,
    /// The first copy of each source inode with several links, so that
    /// later names link to it.
    links: HashMap<Inode, PathBuf>,
    stats: Stats,
    /// Xattrs left out, warned about once at the end.
    skipped: meta::SkippedXattrs,
    /// Special files [`CopyOpts::skip_special`] left out.
    special: SkippedSpecial,
}

/// Device nodes, FIFOs and sockets a copy left out: how many and the first,
/// warned about in one line at the end, never per entry.
#[derive(Debug, Default)]
struct SkippedSpecial {
    count: u64,
    first: Option<PathBuf>,
}

impl SkippedSpecial {
    fn add(&mut self, path: &Path) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some(path.to_owned());
        }
    }

    fn warn(&self, from: &Path) {
        if let Some(first) = &self.first {
            tracing::warn!(
                "rnk-rootfs: the copy of {} left out {} device node(s), FIFO(s) or socket(s), \
                 first {}: kaniko mode makes none in a root a RUN enters without nodev \
                 (ADR-0041, ADR-0060)",
                from.display(),
                self.count,
                first.display()
            );
        }
    }
}

/// One source directory being copied. One frame per level holds two fds,
/// so a deep tree needs a raised `RLIMIT_NOFILE`, as deletion does. Its
/// paths are not kept: the walk shares one source and one destination path
/// that grow by a name on entering a frame and shrink when it is done, so
/// memory is linear in the tree's depth, not in the sum of its paths (T15).
struct Frame {
    /// The source directory, `O_RDONLY | O_DIRECTORY`.
    src: OwnedFd,
    /// What `statx` said of it: the directory its entries were found in.
    stat: Stat,
    /// The destination directory, `O_PATH | O_DIRECTORY`.
    dst: OwnedFd,
    /// The destination's mode and times, set through `dst` when the frame
    /// is done, so that its entries neither move the mtime nor hit a
    /// read-only directory; `None` leaves an existing destination as it is.
    pending: Option<Meta>,
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
                self.walk(sfd, st, dfd, pending, from, &to)
            }
            t if self.o.skip_special && special(t) => {
                self.special.add(from);
                Ok(())
            }
            _ => {
                // For a context's rule, the entry is pinned again together
                // with the directory it is in, both by descriptor, and that
                // pin is what is copied: the directory judged is the one
                // the copied inode was found in.
                let (pinned, st, dir) = if self.o.from_context {
                    let (leaf, again, dir) = src.pin_with_dir(from)?;
                    if again.file_type() != st.file_type() || !again.same_inode(&st) {
                        return Err(Error::InvalidPath {
                            path: from.to_owned(),
                            reason: "changed while it was being copied",
                        });
                    }
                    (leaf, again, Some(dir))
                } else {
                    (pinned, st, None)
                };
                let (parent, name) = path::entry(&to)?;
                let pfd = self.parents(&parent)?;
                self.node(
                    pinned.as_fd(),
                    &st,
                    None,
                    dir.as_ref(),
                    pfd.as_fd(),
                    name,
                    &to,
                )
            }
        }
    }

    fn walk(
        &mut self,
        src: OwnedFd,
        src_stat: Stat,
        dst: OwnedFd,
        pending: Option<Meta>,
        from: &Path,
        to: &Path,
    ) -> Result<()> {
        let names = sorted_names(src.as_fd(), from)?;
        let mut stack = vec![Frame {
            src,
            stat: src_stat,
            dst,
            pending,
            names,
        }];
        // The paths of the entry at hand, shared by every frame: a name is
        // pushed for each entry and popped when it is done, or, for a
        // directory, when its frame is.
        let mut from = from.to_owned();
        let mut to = to.to_owned();
        while let Some(top) = stack.last_mut() {
            let Some(name) = top.names.pop() else {
                // Its entries are done: the directory's own mode and times,
                // through the fd it was written through, deepest first.
                if let Some(frame) = stack.pop() {
                    if let Some(m) = &frame.pending {
                        let h = Handle::Path {
                            fd: frame.dst.as_fd(),
                            file_type: FileType::Directory,
                            parent: None,
                        };
                        meta::apply_lenient(&h, m, &to, &self.skipped)?;
                    }
                    // The first frame is the directory given, never pushed.
                    if !stack.is_empty() {
                        from.pop();
                        to.pop();
                    }
                }
                continue;
            };
            from.push(&name);
            to.push(&name);
            let entered = self.entry(top, &name, &from, &to)?;
            match entered {
                Some(frame) => stack.push(frame),
                None => {
                    from.pop();
                    to.pop();
                }
            }
        }
        Ok(())
    }

    /// One entry `name` of the directory `top`, at `from` and `to`: a
    /// directory gives the frame that copies its entries; anything else is
    /// copied (or left out) here.
    fn entry(
        &mut self,
        top: &Frame,
        name: &OsStr,
        from: &Path,
        to: &Path,
    ) -> Result<Option<Frame>> {
        // Below the source given, never into another mount: a live `/proc`
        // or `/sys` there would be read as files. A mount point that `keep`
        // refuses (kaniko mode copying its root out, with the mounts on its
        // ignore list) is left out instead.
        let pin = match root::open_name(
            top.src.as_fd(),
            name,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            true,
            from,
        ) {
            Ok(pin) => pin,
            Err(e)
                if e.raw_os_error() == Some(Errno::XDEV.raw_os_error())
                    && !(self.keep)(from, true) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(on_mount(e, from)),
        };
        let st = meta::statx_at(pin.as_fd(), OsStr::new(""), from)?;
        if !(self.keep)(from, st.file_type() == FileType::Directory) {
            return Ok(None);
        }
        if self.o.skip_special && special(st.file_type()) {
            self.special.add(from);
            return Ok(None);
        }
        if st.file_type() == FileType::Directory {
            self.check_not_inside(&st, from)?;
            let s = meta::reopen(pin.as_fd(), OFlags::RDONLY | OFlags::DIRECTORY, from)?;
            let (d, mut m) = self.dir_entry(top.dst.as_fd(), name, to, &st)?;
            m.xattrs = meta::read_xattrs_lenient(
                s.as_fd(),
                FileType::Directory,
                None,
                from,
                &self.skipped,
            )?;
            let names = sorted_names(s.as_fd(), from)?;
            return Ok(Some(Frame {
                src: s,
                stat: st,
                dst: d,
                pending: Some(m),
                names,
            }));
        }
        let dir = top.stat;
        let sparent = Some((top.src.as_fd(), name));
        self.node(
            pin.as_fd(),
            &st,
            sparent,
            Some(&dir),
            top.dst.as_fd(),
            name,
            to,
        )?;
        Ok(None)
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
        // BuildKit makes it with `MkdirAll`, owned by `--chown` or else by
        // the process, not by the source directory (`fsutil copy.go:173`).
        if let Some(o) = self.o.owner {
            self.set_owner(fd.as_fd(), pfd.as_fd(), name, o.uid, o.gid, to)?;
        }
        self.stats.dirs += 1;
        let pending = Meta {
            mode: Some(Chmod::created_dir_mode(self.o.chmod.as_ref())),
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
    #[expect(
        clippy::too_many_arguments,
        reason = "a source (fd, stat, parent, directory) and a destination (fd, name, path)"
    )]
    fn node(
        &mut self,
        pin: BorrowedFd<'_>,
        st: &Stat,
        sparent: Option<(BorrowedFd<'_>, &OsStr)>,
        dir: Option<&Stat>,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        to: &Path,
    ) -> Result<()> {
        if self.o.from_context
            && let Some(reason) = dir.and_then(|dir| context_refusal(st, dir))
        {
            return Err(Error::InvalidPath {
                path: to.to_owned(),
                reason,
            });
        }
        // The first copy of a file with other names is remembered by its
        // path, for linking the later ones: past `PATH_MAX` that map would
        // grow by up to a whole deep path per file (a `RUN` can make paths
        // of 100 KB), and the link would fail with `ENAMETOOLONG` anyway.
        // Refused before anything is written; the error names only the
        // entry (T15).
        if st.nlink > 1 && to.as_os_str().len() > PATH_MAX {
            return Err(Error::InvalidPath {
                path: PathBuf::from(name),
                reason: PATH_TOO_LONG,
            });
        }
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
        self.stats.bytes += copy_node(
            pin,
            st,
            sparent,
            Xattrs::Copy,
            Some(&self.skipped),
            pfd,
            name,
            m,
            to,
        )?;
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
                    let dir_mode = Chmod::created_dir_mode(self.o.chmod.as_ref());
                    let created =
                        match rustix::fs::mkdirat(&cur, name, Mode::from_raw_mode(dir_mode)) {
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
                            mode: Some(dir_mode),
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
            Ok(st) if self.dst.off_mount(&st) => Err(crosses_mount(to)),
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
        self.o.chmod.as_ref().map_or(st.permissions(), |m| {
            m.apply(st.permissions(), st.file_type() == FileType::Directory) & meta::PERMISSION_BITS
        })
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
    skipped: Option<&meta::SkippedXattrs>,
    pfd: BorrowedFd<'_>,
    name: &OsStr,
    mut m: Meta,
    path: &Path,
) -> Result<u64> {
    let file_type = st.file_type();
    m.xattrs = match skipped {
        Some(s) => meta::read_xattrs_lenient(pin, file_type, sparent, path, s)?,
        None => meta::read_xattrs(pin, file_type, sparent, which, path)?,
    };
    let set_meta = |h: &Handle<'_>, m: &Meta| match skipped {
        Some(s) => meta::apply_lenient(h, m, path, s),
        None => meta::apply(h, m, path),
    };
    // A symlink or a node made by name: pinned again, never followed.
    let set_meta_at = |pfd: BorrowedFd<'_>, name: &OsStr, m: &Meta| -> Result<()> {
        let fd = root::open_name(pfd, name, OFlags::PATH | OFlags::NOFOLLOW, 0, false, path)?;
        let file_type = meta::statx_at(fd.as_fd(), OsStr::new(""), path)?.file_type();
        let h = Handle::Path {
            fd: fd.as_fd(),
            file_type,
            parent: Some((pfd, name)),
        };
        set_meta(&h, m)
    };
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
            let n = copy_data(src.as_fd(), out.as_fd(), path)?;
            set_meta(&Handle::Open(out.as_fd()), &m)?;
            Ok(n)
        }
        // BuildKit's `copyDevice` clears the socket bit and calls `mknod`,
        // which makes an empty regular file with the socket's mode.
        FileType::Socket => {
            let out = root::open_name(
                pfd,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                0o600,
                true,
                path,
            )?;
            set_meta(&Handle::Open(out.as_fd()), &m)?;
            Ok(0)
        }
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(pin, "", Vec::new())
                .map_err(|e| io_err("readlink", path, e))?;
            rustix::fs::symlinkat(target.as_c_str(), pfd, name)
                .map_err(|e| io_err("symlink", path, e))?;
            m.mode = None;
            set_meta_at(pfd, name, &m).map(|()| 0)
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
            set_meta_at(pfd, name, &m).map(|()| 0)
        }
        FileType::Directory | FileType::Unknown => Err(Error::InvalidPath {
            path: path.to_owned(),
            reason: "only a file, symlink, FIFO, socket or device node is copied as one entry",
        }),
    }
}

/// Copies the content of `src` into the new, empty file `out` and returns
/// its size: a reflink (`FICLONE`) where the filesystem shares extents,
/// else only the data ranges (`SEEK_DATA`/`SEEK_HOLE`), each with
/// `copy_file_range` or reads and writes, the holes left as holes and the
/// size set at the end. A sparse file costs its data, never its size.
/// Without `SEEK_DATA` everything up to the size is data.
///
/// The size is read once, first. A file holding more than that afterwards
/// (one appended to while copied, or a filesystem whose size understates
/// its content) is refused, never cut short; so is one whose size, `mtime`
/// or `ctime` differ at the end from the start (cut short or rewritten
/// meanwhile, which `SEEK_DATA` would read as a hole and copy as zeros):
/// as the walk and the exporter refuse a file that changes under them.
fn copy_data(src: BorrowedFd<'_>, out: BorrowedFd<'_>, path: &Path) -> Result<u64> {
    let err = |op| move |e: Errno| io_err(op, path, e);
    let first = rustix::fs::fstat(src).map_err(err("stat"))?;
    let size = u64::try_from(first.st_size).unwrap_or(0);
    if size > 0 && rustix::fs::ioctl_ficlone(out, src).is_ok() {
        // The clone takes the whole file as it is now.
        let cloned = rustix::fs::fstat(out).map_err(err("stat"))?.st_size;
        return Ok(u64::try_from(cloned).unwrap_or(0));
    }
    let mut at = 0_u64;
    let mut seek = true;
    while at < size {
        let data = if seek {
            match rustix::fs::seek(src, rustix::fs::SeekFrom::Data(at)) {
                Ok(d) => d,
                // No data after `at`: the rest is a hole.
                Err(Errno::NXIO) => break,
                // A filesystem without `SEEK_DATA`: the rest is data.
                Err(Errno::INVAL | Errno::OPNOTSUPP) => {
                    seek = false;
                    at
                }
                Err(e) => return Err(io_err("seek", path, e)),
            }
        } else {
            at
        };
        if data >= size {
            break;
        }
        let hole = if seek {
            match rustix::fs::seek(src, rustix::fs::SeekFrom::Hole(data)) {
                Ok(h) if h > data => h.min(size),
                // No progress, or no `SEEK_HOLE`: the rest is data.
                Ok(_) | Err(Errno::INVAL | Errno::OPNOTSUPP) => size,
                Err(e) => return Err(io_err("seek", path, e)),
            }
        } else {
            size
        };
        copy_range(src, out, data, hole, path)?;
        at = hole;
    }
    rustix::fs::ftruncate(out, size).map_err(err("truncate"))?;
    let mut byte = [0_u8; 1];
    loop {
        match rustix::io::pread(src, &mut byte, size) {
            Ok(0) => break,
            Ok(_) => {
                return Err(Error::InvalidPath {
                    path: path.to_owned(),
                    reason: "the file changed while it was copied: it holds more than its size",
                });
            }
            Err(Errno::INTR) => {}
            Err(e) => return Err(io_err("read", path, e)),
        }
    }
    // A file cut short, or rewritten, while its ranges were copied: a
    // `SEEK_DATA` past the cut reads as a hole and the rest would come out
    // as zeros. The size and times read again must be the ones read first.
    let last = rustix::fs::fstat(src).map_err(err("stat"))?;
    if (
        last.st_size,
        last.st_mtime,
        last.st_mtime_nsec,
        last.st_ctime,
        last.st_ctime_nsec,
    ) != (
        first.st_size,
        first.st_mtime,
        first.st_mtime_nsec,
        first.st_ctime,
        first.st_ctime_nsec,
    ) {
        return Err(Error::InvalidPath {
            path: path.to_owned(),
            reason: "the file changed while it was copied",
        });
    }
    Ok(size)
}

/// Copies bytes `from..to` of `src` to the same offsets of `out`.
fn copy_range(
    src: BorrowedFd<'_>,
    out: BorrowedFd<'_>,
    from: u64,
    to: u64,
    path: &Path,
) -> Result<()> {
    let (mut off_in, mut off_out) = (from, from);
    while off_in < to {
        let want = usize::try_from(to - off_in)
            .unwrap_or(usize::MAX)
            .min(1 << 30);
        match rustix::fs::copy_file_range(src, Some(&mut off_in), out, Some(&mut off_out), want) {
            Ok(0) => {
                return Err(Error::InvalidPath {
                    path: path.to_owned(),
                    reason: "the file shrank while it was copied",
                });
            }
            Ok(_) => {}
            Err(Errno::INTR) => {}
            // Across filesystems on older kernels, or where it is refused:
            // plain reads and writes.
            Err(Errno::XDEV | Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP | Errno::PERM) => {
                return copy_range_rw(src, out, off_in, to, path);
            }
            Err(e) => return Err(io_err("copy_file_range", path, e)),
        }
    }
    Ok(())
}

/// [`copy_range`] with `pread` and `pwrite`.
fn copy_range_rw(
    src: BorrowedFd<'_>,
    out: BorrowedFd<'_>,
    from: u64,
    to: u64,
    path: &Path,
) -> Result<()> {
    let mut buf = vec![0_u8; 1 << 17];
    let mut at = from;
    while at < to {
        let want = usize::try_from(to - at)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = match rustix::io::pread(src, &mut buf[..want], at) {
            Ok(0) => {
                return Err(Error::InvalidPath {
                    path: path.to_owned(),
                    reason: "the file shrank while it was copied",
                });
            }
            Ok(n) => n,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(io_err("read", path, e)),
        };
        let mut done = 0;
        while done < n {
            match rustix::io::pwrite(out, &buf[done..n], at + done as u64) {
                Ok(w) => done += w,
                Err(Errno::INTR) => {}
                Err(e) => return Err(io_err("write", path, e)),
            }
        }
        at += n as u64;
    }
    Ok(())
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
