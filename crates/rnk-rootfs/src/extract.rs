//! Extracting a plain (already decompressed) tar layer into a root.
//!
//! The `tar` crate only parses the stream (ustar, GNU long names, PAX
//! records); every filesystem change goes through `RootPath`. Its `unpack`
//! family, which joins entry names onto a host path, is never used.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType as RawFileType, Mode, OFlags};
use rustix::io::Errno;
use tar::EntryType;

use crate::delete::{self, Counts, Keep};
use crate::error::{Error, Result, io_err};
use crate::meta::{self, FileTime, FileType, Handle, Meta, PERMISSION_BITS, Stat};
use crate::path;
use crate::root::{self, Mounts, RootPath};

/// How OCI whiteouts in a layer are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Whiteouts {
    /// Apply them to what is already in the root, as an extractor flattening
    /// layers onto one tree does: `.wh.<name>` deletes `<name>`, and
    /// `.wh..wh..opq` empties its directory of everything this layer did not
    /// put there. Nothing is written for them.
    ///
    /// Entries on or below a mount inside the root are refused, not skipped:
    /// there is no ignore list here yet, so a live root with mounts (kaniko
    /// mode on `/`) is not supported (Phase 2).
    Apply,
    /// Keep them in overlayfs form, for an extracted layer used as a lower
    /// directory (`layers/<id>`, mounted with `userxattr`): `.wh.<name>`
    /// becomes the character device 0/0 `<name>`, `.wh..wh..opq` the xattr
    /// `user.overlay.opaque=y` on its directory, and content xattrs named
    /// `user.overlay.*` are escaped as `user.overlay.overlay.*` (Linux 6.7).
    Overlay,
}

const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE: &[u8] = b".wh..wh..opq";
/// AUFS bookkeeping (`.wh..wh.plnk`, `.wh..wh.aufs`, ...), skipped.
const AUFS_META_PREFIX: &[u8] = b".wh..wh.";
const OVERLAY_XATTR: &[u8] = b"user.overlay.";
const OVERLAY_OPAQUE: &str = "user.overlay.opaque";
const PAX_XATTR: &[u8] = b"SCHILY.xattr.";
/// Mode of a directory until its own mode is set at the end.
const DIR_PENDING_MODE: u32 = 0o700;
/// Mode of missing parent directories the layer does not list.
const PARENT_MODE: u32 = 0o755;

/// Extracts the plain tar stream `tar` into `dst`.
///
/// Entry names are cleaned lexically; a name or hardlink target whose `..`
/// climbs above the root is refused ([`Error::Entry`]), and everything else
/// is created through [`RootPath`], so symlinks in the root or earlier in the
/// layer cannot redirect an entry outside it. Ownership is the numeric uid
/// and gid of the header; modes are the raw bits (setuid, setgid, sticky);
/// times come from the header or its PAX records; xattrs from
/// `SCHILY.xattr.*` records. An existing entry is replaced, never followed;
/// an existing directory meets a directory entry by merging. Directory modes
/// and times are set last, deepest first.
///
/// The caller decompresses (`rnk-layer`) and publishes the result; when the
/// extraction fails, what was written stays for the caller to delete.
///
/// # Errors
///
/// [`Error::Tar`] for a malformed stream, [`Error::Entry`] for a refused
/// entry, [`Error::Io`] when the filesystem refuses an operation (for
/// example a device node or a foreign owner without privileges).
pub fn extract(dst: &RootPath, tar: impl Read, wh: Whiteouts) -> Result<()> {
    let span = tracing::info_span!(
        "rnk_rootfs.extract",
        whiteouts = ?wh,
        entries = tracing::field::Empty,
        files = tracing::field::Empty,
        dirs = tracing::field::Empty,
        symlinks = tracing::field::Empty,
        hardlinks = tracing::field::Empty,
        nodes = tracing::field::Empty,
        whiteout_entries = tracing::field::Empty,
        bytes = tracing::field::Empty,
    );
    let _entered = span.enter();
    let mut ex = Extractor {
        root: dst,
        wh,
        dirs: BTreeMap::new(),
        created: HashSet::new(),
        ancestors: HashSet::new(),
        stats: Stats::default(),
    };
    let r = ex.run(tar);
    ex.stats.record(&span);
    r
}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    entries: u64,
    files: u64,
    dirs: u64,
    symlinks: u64,
    hardlinks: u64,
    nodes: u64,
    whiteouts: u64,
    bytes: u64,
}

impl Stats {
    fn record(&self, span: &tracing::Span) {
        span.record("entries", self.entries);
        span.record("files", self.files);
        span.record("dirs", self.dirs);
        span.record("symlinks", self.symlinks);
        span.record("hardlinks", self.hardlinks);
        span.record("nodes", self.nodes);
        span.record("whiteout_entries", self.whiteouts);
        span.record("bytes", self.bytes);
    }
}

struct Extractor<'r> {
    root: &'r RootPath,
    wh: Whiteouts,
    /// Directory mode and times, set at the end, deepest first (reverse
    /// path order), so that adding entries does not move a directory's
    /// mtime and a read-only directory is still writable while it fills.
    dirs: BTreeMap<PathBuf, Meta>,
    /// [`Whiteouts::Apply`]: entries this layer created, and their
    /// ancestors, which an opaque whiteout must not remove.
    created: HashSet<PathBuf>,
    ancestors: HashSet<PathBuf>,
    stats: Stats,
}

/// What a tar entry asks for, after cleaning.
enum Kind {
    Dir,
    File,
    Symlink(PathBuf),
    Hardlink(PathBuf),
    Node(RawFileType, u32, u32),
}

impl Extractor<'_> {
    fn run(&mut self, tar: impl Read) -> Result<()> {
        let mut archive = tar::Archive::new(tar);
        for entry in archive.entries().map_err(Error::Tar)? {
            let mut entry = entry.map_err(Error::Tar)?;
            self.stats.entries += 1;
            self.entry(&mut entry)?;
        }
        self.finish()
    }

    fn entry<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<()> {
        let etype = entry.header().entry_type();
        if etype == EntryType::XGlobalHeader {
            return Ok(());
        }
        let raw = PathBuf::from(OsStr::from_bytes(&entry.path_bytes()));
        let refuse = |reason: &str| Error::Entry {
            path: raw.clone(),
            reason: reason.to_owned(),
        };
        let path = path::clean(&raw).ok_or_else(|| refuse("the name leaves the root"))?;

        if let Some(name) = path.file_name()
            && name.as_bytes().starts_with(WHITEOUT_PREFIX)
        {
            return self.whiteout(&path, name.as_bytes(), &raw);
        }

        let kind = match etype {
            EntryType::Directory => Kind::Dir,
            EntryType::Regular | EntryType::Continuous => Kind::File,
            EntryType::GNUSparse => {
                // The `tar` crate fills holes with zeros, so a tiny entry could
                // claim 2^62 bytes and fill the disk. Extension headers stay
                // unbounded for now (Q-056).
                return Err(refuse("GNU sparse entries are not supported"));
            }
            EntryType::Symlink | EntryType::Link => {
                let link = entry
                    .link_name_bytes()
                    .ok_or_else(|| refuse("a link entry without a target"))?;
                let link = PathBuf::from(OsStr::from_bytes(&link));
                if etype == EntryType::Symlink {
                    Kind::Symlink(link)
                } else {
                    let target = path::clean(&link)
                        .ok_or_else(|| refuse("the hardlink target leaves the root"))?;
                    if target == path || target.as_os_str().is_empty() {
                        return Err(refuse("a hardlink to itself or to the root"));
                    }
                    Kind::Hardlink(target)
                }
            }
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                let h = entry.header();
                let major = h.device_major().map_err(Error::Tar)?.unwrap_or(0);
                let minor = h.device_minor().map_err(Error::Tar)?.unwrap_or(0);
                let t = match etype {
                    EntryType::Char => RawFileType::CharacterDevice,
                    EntryType::Block => RawFileType::BlockDevice,
                    _ => RawFileType::Fifo,
                };
                Kind::Node(t, major, minor)
            }
            other => {
                return Err(refuse(&format!(
                    "unsupported entry type {:?}",
                    char::from(other.as_byte())
                )));
            }
        };
        let meta = self.header_meta(entry, &raw)?;

        if path.as_os_str().is_empty() {
            // The layer's root directory: its metadata applies to the root.
            return match kind {
                Kind::Dir => self.dir_meta(&path, None, meta),
                Kind::File | Kind::Symlink(_) | Kind::Hardlink(_) | Kind::Node(..) => {
                    Err(refuse("the root of a layer must be a directory"))
                }
            };
        }
        let (parent, name) = path::entry(&path)?;
        let pfd = self
            .root
            .mkdir_all(&parent, PARENT_MODE, Mounts::Stay)
            .map_err(|e| on_mount(e, &raw))?;
        let name = name.to_owned();
        let is_dir = matches!(kind, Kind::Dir);
        let existing_dir = self.make_room(&path, pfd.as_fd(), &name, is_dir, &raw)?;

        match kind {
            Kind::Dir => {
                if !existing_dir {
                    rustix::fs::mkdirat(&pfd, &name, Mode::from_raw_mode(DIR_PENDING_MODE))
                        .map_err(|e| io_err("mkdir", &path, e))?;
                }
                self.stats.dirs += 1;
                self.dir_meta(&path, Some((pfd.as_fd(), &name)), meta)?;
            }
            Kind::File => {
                let fd = root::open_name(
                    pfd.as_fd(),
                    &name,
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                    0o600,
                    false,
                    &path,
                )?;
                let mut file = std::fs::File::from(fd);
                let n = std::io::copy(entry, &mut file).map_err(|source| Error::Io {
                    op: "write",
                    path: path.clone(),
                    source,
                })?;
                self.stats.bytes += n;
                self.stats.files += 1;
                meta::apply(&Handle::Open(file.as_fd()), &meta, &path)?;
            }
            Kind::Symlink(target) => {
                rustix::fs::symlinkat(&target, &pfd, &name)
                    .map_err(|e| io_err("symlink", &path, e))?;
                self.stats.symlinks += 1;
                let m = Meta { mode: None, ..meta };
                root::set_meta_at(pfd.as_fd(), &name, &m, None, &path)?;
            }
            Kind::Hardlink(target) => {
                // [open: Q-055] In the overlay form the root holds only this
                // layer, so a target in a lower layer is missing.
                let missing = || Error::Entry {
                    path: raw.clone(),
                    reason: format!(
                        "the hardlink target {} is neither in this layer nor in the root",
                        target.display()
                    ),
                };
                let is_noent = |e: &Error| e.raw_os_error() == Some(Errno::NOENT.raw_os_error());
                let (tparent, tname) = path::entry(&target)?;
                let tpfd = match self.root.resolve_dir_in(&tparent, Mounts::Stay) {
                    Ok(fd) => fd,
                    Err(e) if is_noent(&e) => return Err(missing()),
                    Err(e) => return Err(on_mount(e, &raw)),
                };
                // Flags 0: a symlink target is linked itself, not followed.
                match rustix::fs::linkat(&tpfd, tname, &pfd, &name, AtFlags::empty()) {
                    Ok(()) => {}
                    Err(Errno::NOENT) => return Err(missing()),
                    Err(e) => return Err(io_err("link", &path, e)),
                }
                self.stats.hardlinks += 1;
            }
            Kind::Node(t, major, minor) => {
                rustix::fs::mknodat(
                    &pfd,
                    &name,
                    t,
                    Mode::from_raw_mode(0o600),
                    rustix::fs::makedev(major, minor),
                )
                .map_err(|e| io_err("mknod", &path, e))?;
                self.stats.nodes += 1;
                root::set_meta_at(pfd.as_fd(), &name, &meta, None, &path)?;
            }
        }
        self.mark_created(path);
        Ok(())
    }

    /// Ownership, xattrs, raw mode and times from the header and its PAX
    /// records.
    fn header_meta<R: Read>(&self, entry: &mut tar::Entry<'_, R>, raw: &Path) -> Result<Meta> {
        let refuse = |reason: String| Error::Entry {
            path: raw.to_owned(),
            reason,
        };
        let h = entry.header();
        let id = |v: u64, what: &str| {
            u32::try_from(v).map_err(|_| refuse(format!("{what} {v} is out of range")))
        };
        let uid = id(h.uid().map_err(Error::Tar)?, "uid")?;
        let gid = id(h.gid().map_err(Error::Tar)?, "gid")?;
        let mode = h.mode().map_err(Error::Tar)? & PERMISSION_BITS;
        let mtime = h.mtime().map_err(Error::Tar)?;
        let mut mtime = FileTime::from_secs(
            i64::try_from(mtime).map_err(|_| refuse(format!("mtime {mtime} is out of range")))?,
        );
        let mut atime = None;
        let mut xattrs = Vec::new();
        if let Some(pax) = entry.pax_extensions().map_err(Error::Tar)? {
            for ext in pax {
                let ext = ext.map_err(Error::Tar)?;
                let key = ext.key_bytes();
                if let Some(name) = key.strip_prefix(PAX_XATTR) {
                    xattrs.push((self.xattr_name(name), ext.value_bytes().to_vec()));
                } else if key == b"mtime" {
                    mtime = pax_time(ext.value_bytes())
                        .ok_or_else(|| refuse("malformed PAX mtime".to_owned()))?;
                } else if key == b"atime" {
                    atime = Some(
                        pax_time(ext.value_bytes())
                            .ok_or_else(|| refuse("malformed PAX atime".to_owned()))?,
                    );
                }
            }
        }
        Ok(Meta {
            uid: Some(uid),
            gid: Some(gid),
            mode: Some(mode),
            atime: Some(atime.unwrap_or(mtime)),
            mtime: Some(mtime),
            xattrs,
        })
    }

    /// The name an xattr from the layer is stored under.
    fn xattr_name(&self, name: &[u8]) -> OsString {
        match self.wh {
            Whiteouts::Overlay if name.starts_with(OVERLAY_XATTR) => {
                // Overlay hides `user.overlay.*`; since Linux 6.7 it shows
                // `user.overlay.overlay.X` as `user.overlay.X`.
                let mut escaped = OVERLAY_XATTR.to_vec();
                escaped.extend_from_slice(b"overlay.");
                escaped.extend_from_slice(&name[OVERLAY_XATTR.len()..]);
                OsStr::from_bytes(&escaped).to_owned()
            }
            Whiteouts::Overlay | Whiteouts::Apply => OsStr::from_bytes(name).to_owned(),
        }
    }

    /// Owner and xattrs of a directory now; mode and times at the end.
    /// `at` is the directory's parent and name; `None` for the root.
    fn dir_meta(
        &mut self,
        path: &Path,
        at: Option<(BorrowedFd<'_>, &OsStr)>,
        meta: Meta,
    ) -> Result<()> {
        let now = Meta {
            uid: meta.uid,
            gid: meta.gid,
            xattrs: meta.xattrs,
            ..Meta::default()
        };
        match at {
            Some((pfd, name)) => root::set_meta_at(pfd, name, &now, None, path).map(drop)?,
            None => self.root.set_meta(path, &now)?,
        }
        self.dirs.insert(
            path.to_owned(),
            Meta {
                mode: meta.mode,
                atime: meta.atime,
                mtime: meta.mtime,
                ..Meta::default()
            },
        );
        Ok(())
    }

    /// Clears the name `name` in `pfd` for a new entry at `path`. Returns
    /// whether a directory is already there and the new entry, a directory
    /// too, merges into it. A mount point is never replaced or merged into.
    fn make_room(
        &mut self,
        path: &Path,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        dir: bool,
        raw: &Path,
    ) -> Result<bool> {
        let st = match meta::statx_at(pfd, name, path) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(false),
            Err(e) => return Err(e),
        };
        if st.mnt_id != self.root.mnt_id() || st.mount_root {
            return Err(crosses_mount(raw));
        }
        let is_dir = st.file_type() == FileType::Directory;
        if is_dir && dir {
            return Ok(true);
        }
        if is_dir {
            self.remove_dir_tree(path, pfd, name, &st)?;
        } else {
            rustix::fs::unlinkat(pfd, name, AtFlags::empty())
                .map_err(|e| io_err("unlink", path, e))?;
        }
        Ok(false)
    }

    /// Removes the directory `name` in `pfd` (at `path`, as `st` described
    /// it) and everything in it, through the deletion walker started from
    /// that very directory; forgets its pending metadata.
    fn remove_dir_tree(
        &mut self,
        path: &Path,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        st: &Stat,
    ) -> Result<()> {
        let mut counts = Counts::default();
        let start = delete::open_dir_checked(pfd, name, st, path)?;
        let all = delete::remove_below(
            self.root,
            start,
            path,
            &mut |_, _| Keep::Remove,
            &mut counts,
        )?;
        if !all {
            return Err(Error::Entry {
                path: path.to_owned(),
                reason: "replaces a directory holding a mount point".to_owned(),
            });
        }
        rustix::fs::unlinkat(pfd, name, AtFlags::REMOVEDIR)
            .map_err(|e| io_err("rmdir", path, e))?;
        self.dirs.retain(|p, _| !p.starts_with(path));
        Ok(())
    }

    fn whiteout(&mut self, path: &Path, name: &[u8], raw: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new("")).to_owned();
        if name == OPAQUE {
            self.stats.whiteouts += 1;
            return match self.wh {
                Whiteouts::Overlay => {
                    let dfd = self
                        .root
                        .mkdir_all(&dir, PARENT_MODE, Mounts::Stay)
                        .map_err(|e| on_mount(e, raw))?;
                    let m = Meta {
                        xattrs: vec![(OsString::from(OVERLAY_OPAQUE), b"y".to_vec())],
                        ..Meta::default()
                    };
                    let h = Handle::Path {
                        fd: dfd.as_fd(),
                        file_type: FileType::Directory,
                        parent: None,
                    };
                    meta::apply(&h, &m, &dir)
                }
                Whiteouts::Apply => self.apply_opaque(&dir, raw),
            };
        }
        if name.starts_with(AUFS_META_PREFIX) {
            return Ok(());
        }
        let hidden = OsStr::from_bytes(&name[WHITEOUT_PREFIX.len()..]);
        if hidden.is_empty() || hidden == "." || hidden == ".." {
            return Err(Error::Entry {
                path: raw.to_owned(),
                reason: "a whiteout must name an entry".to_owned(),
            });
        }
        let target = dir.join(hidden);
        self.stats.whiteouts += 1;
        match self.wh {
            Whiteouts::Overlay => {
                let pfd = self
                    .root
                    .mkdir_all(&dir, PARENT_MODE, Mounts::Stay)
                    .map_err(|e| on_mount(e, raw))?;
                self.make_room(&target, pfd.as_fd(), hidden, false, raw)?;
                rustix::fs::mknodat(
                    &pfd,
                    hidden,
                    RawFileType::CharacterDevice,
                    Mode::empty(),
                    rustix::fs::makedev(0, 0),
                )
                .map_err(|e| io_err("mknod whiteout", &target, e))
            }
            Whiteouts::Apply => {
                let pfd = match self.root.resolve_dir_in(&dir, Mounts::Stay) {
                    Ok(fd) => fd,
                    // Nothing below a missing directory to hide.
                    Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                        return Ok(());
                    }
                    Err(e) => return Err(on_mount(e, raw)),
                };
                match meta::statx_at(pfd.as_fd(), hidden, &target) {
                    Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => Ok(()),
                    Err(e) => Err(e),
                    Ok(st) if st.mnt_id != self.root.mnt_id() || st.mount_root => {
                        Err(crosses_mount(raw))
                    }
                    Ok(st) if st.file_type() == FileType::Directory => {
                        self.remove_dir_tree(&target, pfd.as_fd(), hidden, &st)
                    }
                    Ok(_) => rustix::fs::unlinkat(&pfd, hidden, AtFlags::empty())
                        .map_err(|e| io_err("unlink", &target, e)),
                }
            }
        }
    }

    /// Empties `dir` of everything this layer did not create.
    fn apply_opaque(&mut self, dir: &Path, raw: &Path) -> Result<()> {
        let (created, ancestors) = (&self.created, &self.ancestors);
        let mut decide = |p: &Path, t: FileType| {
            if created.contains(p) {
                if t == FileType::Directory {
                    Keep::Descend
                } else {
                    Keep::Whole
                }
            } else if ancestors.contains(p) {
                Keep::Descend
            } else {
                Keep::Remove
            }
        };
        let start = match delete::open_dir(self.root, dir) {
            Ok(fd) => fd,
            // A directory missing from the root hides nothing. Only the
            // start may be missing: an `ENOENT` during the walk is an error.
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(()),
            Err(e) => return Err(on_mount(e, raw)),
        };
        // Pending directory metadata needs no cleanup: it belongs to
        // directories of this layer, which are all kept.
        let mut counts = Counts::default();
        delete::remove_below(self.root, start, dir, &mut decide, &mut counts).map(drop)
    }

    /// Records `path` as created by this layer, for opaque whiteouts.
    fn mark_created(&mut self, path: PathBuf) {
        if self.wh != Whiteouts::Apply {
            return;
        }
        let mut a = path.parent();
        while let Some(p) = a {
            if p.as_os_str().is_empty() || !self.ancestors.insert(p.to_owned()) {
                break;
            }
            a = p.parent();
        }
        self.created.insert(path);
    }

    /// Directory modes and times, deepest first.
    fn finish(&mut self) -> Result<()> {
        for (path, m) in self.dirs.iter().rev() {
            // A pending directory that a later entry replaced was dropped
            // from the map; the type check on the pinned entry only guards
            // against a change made outside the extraction.
            self.root.set_dir_meta(path, m)?;
        }
        Ok(())
    }
}

/// The error for an entry on or below a mount point inside the root.
fn crosses_mount(raw: &Path) -> Error {
    Error::Entry {
        path: raw.to_owned(),
        reason: "lies on or below a mount point inside the root; extraction never crosses a mount"
            .to_owned(),
    }
}

/// Maps `EXDEV` from a [`Mounts::Stay`] resolution to [`crosses_mount`].
fn on_mount(e: Error, raw: &Path) -> Error {
    if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) {
        crosses_mount(raw)
    } else {
        e
    }
}

/// A PAX time: decimal seconds, optionally negative, with an optional
/// fraction of up to nine digits (more are truncated).
fn pax_time(v: &[u8]) -> Option<FileTime> {
    let s = std::str::from_utf8(v).ok()?;
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: i64 = int.parse().ok()?;
    let digits = frac.get(..frac.len().min(9)).unwrap_or("");
    let mut nanos: u32 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    for _ in digits.len()..9 {
        nanos *= 10;
    }
    if !neg {
        return Some(FileTime { secs, nanos });
    }
    if nanos == 0 {
        return Some(FileTime {
            secs: -secs,
            nanos: 0,
        });
    }
    Some(FileTime {
        secs: -secs - 1,
        nanos: 1_000_000_000 - nanos,
    })
}

#[cfg(test)]
mod tests;
