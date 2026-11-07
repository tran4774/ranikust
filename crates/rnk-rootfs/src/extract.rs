//! Extracting a plain (already decompressed) tar layer into a root.
//!
//! The `tar` crate only parses the stream (ustar, GNU long names, PAX
//! records); every filesystem change goes through `RootPath`. Its `unpack`
//! family, which joins entry names onto a host path, is never used.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::ops::Bound;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType as RawFileType, Mode, OFlags};
use rustix::io::Errno;
use tar::EntryType;

use crate::copy;
use crate::delete::{self, Counts, Keep};
use crate::error::{Error, Result, io_err};
use crate::ignore::IgnoreList;
use crate::meta::{self, FileTime, FileType, Handle, Meta, PERMISSION_BITS, Stat, Xattrs};
use crate::path;
use crate::root::{self, Mounts, RootPath};

mod created;
mod limit;

use created::{Created, Mark};
pub use limit::{DEFAULT_EXTENSION_HEADER_LIMIT, set_extension_header_limit};

/// How OCI whiteouts in a layer are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Whiteouts {
    /// Apply them to what is already in the root, as an extractor flattening
    /// layers onto one tree does: `.wh.<name>` deletes `<name>`, and
    /// `.wh..wh..opq` empties its directory of everything this layer did not
    /// put there. Nothing is written for them.
    ///
    /// [`extract`] refuses entries on or below a mount inside the root;
    /// [`extract_live`] skips them, and the paths of its ignore list.
    Apply,
    /// Keep them in overlayfs form, for an extracted layer used as a lower
    /// directory (`layers/<id>`, mounted with `userxattr`): `.wh.<name>`
    /// becomes the character device 0/0 `<name>`, `.wh..wh..opq` the xattr
    /// `user.overlay.opaque=y` on its directory, and content xattrs named
    /// `user.overlay.*` are escaped as `user.overlay.overlay.*` (Linux 6.7).
    Overlay,
    /// No whiteouts: every name is taken literally, a file named
    /// `.wh.<x>` included, for a tar that is a snapshot of a tree rather
    /// than an OCI layer (a kaniko-mode stage kept for a later one,
    /// ADR-0056). Otherwise as [`Whiteouts::Apply`].
    Literal,
}

/// What [`extract_overlay`] does with a layer's block and character device
/// nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceNodes {
    /// Creates them, as overlay mode as root does.
    Create,
    /// Creates none, nor a hardlink to one the same layer left out while
    /// its name is remembered, and counts each as `nodes_skipped`, for a
    /// process in a user namespace, where `mknod` of a device needs
    /// `CAP_MKNOD` in the initial one (ADR-0106). FIFOs and whiteouts are
    /// still made. A link to a device whose name came past a budget for the
    /// names remembered (ADR-0128), or to one a lower layer left out
    /// (ADR-0131), is linked to what the layer has at that name, else
    /// copied from the nearest lower layer that shows it (ADR-0012), else
    /// refused.
    Skip,
}

const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE: &[u8] = b".wh..wh..opq";
/// AUFS bookkeeping (`.wh..wh.plnk`, `.wh..wh.aufs`, ...), skipped.
const AUFS_META_PREFIX: &[u8] = b".wh..wh.";
const OVERLAY_XATTR: &[u8] = b"user.overlay.";
const OVERLAY_OPAQUE: &str = "user.overlay.opaque";
const PAX_XATTR: &[u8] = b"SCHILY.xattr.";
/// Xattrs a layer never sets (`ADR-0075`), from its tar or copied up from a
/// lower layer ([`meta::Xattrs::Layer`]).
const SKIPPED_XATTR: &[u8] = meta::LAYER_SKIPPED_XATTR;
/// Mode of a directory until its own mode is set at the end.
const DIR_PENDING_MODE: u32 = 0o700;
/// The bytes [`Extractor::remember_skipped`] spends at most per layer on
/// the names of devices left out, which take no room on disk.
const SKIPPED_NAMES_MAX: usize = 1 << 20;
/// What one remembered name costs besides its own bytes: its `PathBuf` and
/// its share of the set's nodes.
const SKIPPED_NAME_COST: usize = 64;
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
/// `SCHILY.xattr.*` records, except `trusted.*`, which is never set
/// (`ADR-0075`), only counted and warned about once per layer. An existing
/// entry is replaced, never followed;
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
    extract_with(dst, tar, wh, &[], None, DeviceNodes::Create).map(drop)
}

/// Extracts the plain tar stream `tar` into a live root, as kaniko mode
/// extracts a base image into the root it builds in
/// (`kaniko@44aa7ddc0 pkg/util/fs_util.go:209-278`, `:410-433`): as
/// [`extract`] with [`Whiteouts::Apply`], except that what `keep` covers
/// and what lies on another mount are left alone instead of refused.
///
/// Each entry is checked at its physical path, where it lands once the
/// symlinks of the root are followed, not only by its name: a layer
/// cannot write into an ignored directory (the state directory, `/proc`)
/// through a symlink to it. Skipped, and counted as `ignored`:
///
/// - an entry at or below an ignored path, or on or below a mount point;
/// - a hardlink whose target is ignored or on another mount;
/// - an entry that would replace a directory holding an ignored path or a
///   mount point: what is not ignored in that directory is removed first,
///   as kaniko's `FF_KANIKO_PRESERVE_MOUNTED_PATHS` does (`:343-370`);
/// - a whiteout of an ignored path or of a directory holding one, as
///   kaniko skips it (`:248-266`).
///
/// Device nodes and FIFOs are not created at all, as kaniko's `ExtractFile`
/// creates none (`:440-575`, ADR-0041); they are counted as
/// `nodes_skipped`. The image keeps them: its base layers are reused
/// unchanged.
///
/// An opaque whiteout empties its directory of everything the layer did not
/// put there, except ignored paths and mounts.
///
/// # Errors
///
/// As [`extract`]; [`Error::Entry`] when the physical path of a directory
/// cannot be told (`/proc/self/fd` gives a path outside the root).
pub fn extract_live(dst: &RootPath, tar: impl Read, keep: &IgnoreList) -> Result<()> {
    extract_with(
        dst,
        tar,
        Whiteouts::Apply,
        &[],
        Some(keep),
        DeviceNodes::Skip,
    )
    .map(drop)
}

/// [`extract_live`] with [`Whiteouts::Literal`]: a snapshot of a tree, not
/// a layer, written back into a live root (a kaniko-mode stage built on a
/// kept one, ADR-0056); a name `.wh.<x>` is a file.
///
/// # Errors
///
/// As [`extract_live`].
pub fn extract_live_literal(dst: &RootPath, tar: impl Read, keep: &IgnoreList) -> Result<()> {
    extract_with(
        dst,
        tar,
        Whiteouts::Literal,
        &[],
        Some(keep),
        DeviceNodes::Skip,
    )
    .map(drop)
}

/// Extracts the plain tar stream `tar` into `dst` in the overlay form
/// ([`Whiteouts::Overlay`]), as one layer above `lowers`: the extracted
/// directories of the layers below it, the one right below first, as in
/// overlay's `lowerdir=`.
///
/// Everything is as [`extract`], except a hardlink whose target is not in
/// this layer: the target is looked up in `lowers` as the merged view
/// would show it (a whiteout, an opaque directory or a non-directory on the
/// way in a higher layer hides it) and copied as a regular file, owner,
/// mode, times and xattrs included, which splits the hardlink as overlay's
/// copy-up does (Q-055, `ADR-0012`). Overlay's own xattrs on the lower
/// inode (`trusted.overlay.*`, `user.overlay.*`) are left behind; the
/// escaped content xattrs `user.overlay.overlay.*` are kept. Further
/// hardlinks in this layer to the same lower inode, by any of its names,
/// link to that copy, so the bytes written stay bounded by the lower
/// layers' size. The lower layers are only read.
///
/// `devices` tells whether block and character device nodes are made or
/// left out ([`DeviceNodes::Skip`], ADR-0106), as if the tar did not hold
/// them, apart from a hardlink to one, left out with it while its name is
/// remembered (ADR-0128). One as the layer's root is refused either way.
///
/// Returns what the caller cannot see on disk afterwards: whether the layer
/// set the root's metadata (an entry `./`). Without one, the root of an
/// image keeps what the layers below gave it, so the caller takes it from
/// them.
///
/// # Errors
///
/// As [`extract`]; [`Error::Entry`] when the target is in no layer or hidden,
/// is a directory, or lies below a symlink in a lower layer.
pub fn extract_overlay(
    dst: &RootPath,
    tar: impl Read,
    lowers: &[&RootPath],
    devices: DeviceNodes,
) -> Result<Extracted> {
    extract_with(dst, tar, Whiteouts::Overlay, lowers, None, devices)
}

/// What [`extract_overlay`] reports about a layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Extracted {
    /// The layer has an entry for its root (`./`), whose metadata the root
    /// now has.
    pub root_set: bool,
}

/// Refuses what an extraction that applies whiteouts would refuse from
/// the headers of the plain tar stream `tar` alone, and writes nothing.
/// Values only the kernel refuses (a NUL byte or an over-long name, an
/// xattr it does not take) still fail at write time (#179).
///
/// It reads as [`extract`], [`extract_live`] and [`extract_overlay`] do:
/// the same framing, up to the end-of-archive block, and the same
/// `tar-header` limit (ADR-0079). It refuses a name or hardlink target that
/// leaves the root, a hardlink to itself or whose target is the layer's
/// root, a link whose header names no target (an empty ustar link field
/// and no PAX `linkpath` or GNU long link), a GNU sparse or unknown entry
/// type, a whiteout that names nothing, a file or symlink as the layer's
/// root, and ids, modes, device numbers and times that do not parse or are
/// out of range.
/// It refuses nothing such an extraction takes: what depends on the root
/// or the ignore list (where an entry lands, a hardlink's target, a
/// hardlink or device node as the layer's root) is left to the extraction.
/// What follows the end-of-archive block is the caller's to read.
///
/// Public for `rnk-snapshot`'s spool pass on `/` (ADR-0123): the pass that
/// decompresses a streamed layer to check its diff ID calls this on the
/// same bytes, so a header it refuses fails the layer before `/` changes,
/// with no second decompression and no tar parser of that crate's own that
/// could take or refuse what the extraction does not.
///
/// # Errors
///
/// As [`extract`]: [`Error::Tar`] for a malformed or unreadable stream,
/// [`Error::ExtensionHeader`] past the `tar-header` limit, [`Error::Entry`]
/// for a refused entry.
pub fn check_headers(tar: impl Read) -> Result<()> {
    let (mut skipped, mut first_skipped) = (0, None);
    for_each_entry(tar, |entry| {
        read_header(entry, Whiteouts::Apply, &mut skipped, &mut first_skipped).map(drop)
    })
}

fn extract_with(
    dst: &RootPath,
    tar: impl Read,
    wh: Whiteouts,
    lowers: &[&RootPath],
    keep: Option<&IgnoreList>,
    devices: DeviceNodes,
) -> Result<Extracted> {
    let span = tracing::info_span!(
        "rnk_rootfs.extract",
        mode = if keep.is_some() {
            "Live"
        } else {
            mode_name(wh)
        },
        lowers = lowers.len(),
        entries = tracing::field::Empty,
        files = tracing::field::Empty,
        dirs = tracing::field::Empty,
        symlinks = tracing::field::Empty,
        hardlinks = tracing::field::Empty,
        hardlinks_copied = tracing::field::Empty,
        nodes = tracing::field::Empty,
        nodes_skipped = tracing::field::Empty,
        xattrs_skipped = tracing::field::Empty,
        whiteouts = tracing::field::Empty,
        ignored = tracing::field::Empty,
        bytes_in = tracing::field::Empty,
        bytes_written = tracing::field::Empty,
    );
    let _entered = span.enter();
    let live = match keep {
        Some(keep) => Some(Live::new(dst, keep)?),
        None => None,
    };
    let mut ex = Extractor {
        root: dst,
        wh,
        lowers,
        live,
        devices,
        dirs: BTreeMap::new(),
        created: Created::default(),
        copied: HashMap::new(),
        copy_names: BTreeMap::new(),
        devices_skipped: BTreeSet::new(),
        skipped_bytes: 0,
        stats: Stats::default(),
        first_skipped: None,
    };
    let mut tar = Counting { inner: tar, n: 0 };
    let r = ex.run(&mut tar);
    ex.stats.bytes_in = tar.n;
    ex.stats.record(&span);
    if let Some((path, name)) = &ex.first_skipped {
        tracing::warn!(
            "{} trusted.* xattr(s) of the layer not set, as BuildKit leaves them out; the first: {}: {}",
            ex.stats.xattrs_skipped,
            path.display(),
            name.to_string_lossy().escape_debug()
        );
    }
    r?;
    Ok(Extracted {
        root_set: ex.dirs.contains_key(Path::new("")),
    })
}

/// Counts the bytes read from the tar stream.
struct Counting<R> {
    inner: R,
    n: u64,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.n += n as u64;
        Ok(n)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    entries: u64,
    files: u64,
    dirs: u64,
    symlinks: u64,
    hardlinks: u64,
    hardlinks_copied: u64,
    nodes: u64,
    nodes_skipped: u64,
    xattrs_skipped: u64,
    whiteouts: u64,
    ignored: u64,
    bytes_in: u64,
    bytes_written: u64,
}

impl Stats {
    fn record(&self, span: &tracing::Span) {
        span.record("entries", self.entries);
        span.record("files", self.files);
        span.record("dirs", self.dirs);
        span.record("symlinks", self.symlinks);
        span.record("hardlinks", self.hardlinks);
        span.record("hardlinks_copied", self.hardlinks_copied);
        span.record("nodes", self.nodes);
        span.record("nodes_skipped", self.nodes_skipped);
        span.record("xattrs_skipped", self.xattrs_skipped);
        span.record("whiteouts", self.whiteouts);
        span.record("ignored", self.ignored);
        span.record("bytes_in", self.bytes_in);
        span.record("bytes_written", self.bytes_written);
    }
}

struct Extractor<'r> {
    root: &'r RootPath,
    wh: Whiteouts,
    /// [`extract_overlay`]: the layers below, the nearest first.
    lowers: &'r [&'r RootPath],
    /// [`extract_live`]: the ignore list and physical paths.
    live: Option<Live<'r>>,
    /// Whether block and character device nodes are made.
    devices: DeviceNodes,
    /// Lower inodes copied for a hardlink, by index in `lowers`, device and
    /// inode, and where the copy is with its `statx`, so that every further
    /// link to any of their names links to it while it is still there.
    copied: HashMap<LowerInode, (PathBuf, Stat)>,
    /// The same copies by name: a copy the layer removes or replaces is
    /// forgotten ([`Extractor::forget_copies`]).
    copy_names: BTreeMap<PathBuf, LowerInode>,
    /// [`DeviceNodes::Skip`]: the names of the devices left out, and of the
    /// hardlinks to them, until the layer puts something else there.
    devices_skipped: BTreeSet<PathBuf>,
    /// The bytes charged for `devices_skipped`, never refunded; past
    /// [`SKIPPED_NAMES_MAX`] once the budget is closed.
    skipped_bytes: usize,
    /// Directory mode and times, set at the end, deepest first (reverse
    /// path order), so that adding entries does not move a directory's
    /// mtime and a read-only directory is still writable while it fills.
    dirs: BTreeMap<PathBuf, Meta>,
    /// [`Whiteouts::Apply`]: entries this layer created and still has, and
    /// their ancestors, which an opaque whiteout must not remove. Each name
    /// is kept once, and what the layer deletes is forgotten, so this grows
    /// with what the layer leaves on disk, not with its entries.
    created: Created,
    stats: Stats,
    /// The first xattr left out ([`SKIPPED_XATTR`]): the entry's name in
    /// the layer and the xattr's, for the one warning per layer.
    first_skipped: Option<(PathBuf, OsString)>,
}

/// A lower layer's inode: its index in `lowers`, device and inode number.
type LowerInode = (usize, u32, u32, u64);

/// What a tar entry asks for, after cleaning.
enum Kind {
    Dir,
    File,
    Symlink(PathBuf),
    Hardlink(PathBuf),
    Node(RawFileType, u32, u32),
}

/// Why a layer's root entry (`./`) that is not a directory is refused.
const ROOT_NOT_DIR: &str = "the root of a layer must be a directory";

/// What an entry's header shows on its own, before the root is looked at
/// ([`read_header`]).
enum Header {
    /// A PAX global header, which sets nothing here.
    Global,
    /// A whiteout: its name in the layer, its cleaned path and what its
    /// name stands for.
    Whiteout {
        raw: PathBuf,
        path: PathBuf,
        name: WhiteoutName,
    },
    /// Any other entry: its name in the layer, its cleaned path, what it
    /// asks for and its metadata.
    Entry {
        raw: PathBuf,
        path: PathBuf,
        kind: Kind,
        meta: Meta,
    },
}

/// Each entry of the plain tar stream `tar`, as every extraction and
/// [`check_headers`] read it: framed by [`limit::Framed`] under the
/// `tar-header` limit (ADR-0079), by the `tar` crate, up to the
/// end-of-archive block.
fn for_each_entry<R: Read>(
    tar: R,
    mut each: impl FnMut(&mut tar::Entry<'_, limit::Framed<R>>) -> Result<()>,
) -> Result<()> {
    let tar = limit::Framed::new(tar, limit::extension_header_limit());
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries().map_err(tar_error)? {
        let mut entry = entry.map_err(tar_error)?;
        each(&mut entry)?;
    }
    Ok(())
}

/// Reads `entry`'s header and PAX records, and refuses what no root can
/// make right: a GNU sparse entry, a name or hardlink target that leaves
/// the root, a hardlink to itself or whose target is the layer's root, a
/// link whose header names no target (an empty ustar link field and no PAX
/// `linkpath` or GNU long link), a whiteout that names nothing, an unknown
/// entry type, a file or symlink as the layer's root, and ids, modes,
/// device numbers and times that do not parse or are out of range. The
/// first step of every extraction, and all of [`check_headers`]. `wh` tells
/// what a whiteout is; the `trusted.*` xattrs left out ([`SKIPPED_XATTR`])
/// are counted in `skipped`, the first of them kept in `first_skipped`.
fn read_header<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    wh: Whiteouts,
    skipped: &mut u64,
    first_skipped: &mut Option<(PathBuf, OsString)>,
) -> Result<Header> {
    let etype = entry.header().entry_type();
    if etype == EntryType::XGlobalHeader {
        return Ok(Header::Global);
    }
    let raw = PathBuf::from(OsStr::from_bytes(&entry.path_bytes()));
    let refuse = |reason: &str| Error::Entry {
        path: raw.clone(),
        reason: reason.to_owned(),
    };
    // Before anything else, a whiteout name included: `limit::Framed`
    // stops following the framing at a sparse entry, so nothing after
    // one may be read.
    if etype == EntryType::GNUSparse {
        return Err(refuse("GNU sparse entries are not supported"));
    }
    let path = path::clean(&raw).ok_or_else(|| refuse("the name leaves the root"))?;

    if let Some(name) = path.file_name()
        && wh != Whiteouts::Literal
        && name.as_bytes().starts_with(WHITEOUT_PREFIX)
    {
        let name = whiteout_name(name.as_bytes(), &raw)?;
        return Ok(Header::Whiteout { raw, path, name });
    }

    let kind = match etype {
        EntryType::Directory => Kind::Dir,
        EntryType::Regular | EntryType::Continuous => Kind::File,
        EntryType::GNUSparse => {
            // The `tar` crate fills holes with zeros, so a tiny entry could
            // claim 2^62 bytes and fill the disk. Refused above already,
            // before the whiteout names.
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
    let meta = header_meta(entry, &raw, wh, skipped, first_skipped)?;
    // A hardlink or a device node there is refused later: a live root may
    // skip it first.
    if path.as_os_str().is_empty() && matches!(kind, Kind::File | Kind::Symlink(_)) {
        return Err(refuse(ROOT_NOT_DIR));
    }
    Ok(Header::Entry {
        raw,
        path,
        kind,
        meta,
    })
}

/// Ownership, xattrs, raw mode and times from the header and its PAX
/// records. Xattrs named [`SKIPPED_XATTR`] are left out and counted.
fn header_meta<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    raw: &Path,
    wh: Whiteouts,
    skipped: &mut u64,
    first_skipped: &mut Option<(PathBuf, OsString)>,
) -> Result<Meta> {
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
                if name.starts_with(SKIPPED_XATTR) {
                    *skipped += 1;
                    if first_skipped.is_none() {
                        *first_skipped = Some((raw.to_owned(), OsStr::from_bytes(name).to_owned()));
                    }
                    continue;
                }
                xattrs.push((xattr_name(wh, name), ext.value_bytes().to_vec()));
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

/// The name an xattr from the layer is stored under in the form `wh`.
fn xattr_name(wh: Whiteouts, name: &[u8]) -> OsString {
    match wh {
        Whiteouts::Overlay if name.starts_with(OVERLAY_XATTR) => {
            // Overlay hides `user.overlay.*`; since Linux 6.7 it shows
            // `user.overlay.overlay.X` as `user.overlay.X`.
            let mut escaped = OVERLAY_XATTR.to_vec();
            escaped.extend_from_slice(b"overlay.");
            escaped.extend_from_slice(&name[OVERLAY_XATTR.len()..]);
            OsStr::from_bytes(&escaped).to_owned()
        }
        Whiteouts::Overlay | Whiteouts::Apply | Whiteouts::Literal => {
            OsStr::from_bytes(name).to_owned()
        }
    }
}

/// What a whiteout's name (`.wh.` and more) stands for.
enum WhiteoutName {
    /// The opaque marker: what the layers below have in its directory is
    /// hidden.
    Opaque,
    /// Other AUFS metadata (`.wh..wh.*`), which hides nothing.
    Meta,
    /// The name of the entry hidden.
    Hides(OsString),
}

/// Reads the whiteout name `name` of the entry `raw`: refused when it
/// names no entry.
fn whiteout_name(name: &[u8], raw: &Path) -> Result<WhiteoutName> {
    if name == OPAQUE {
        return Ok(WhiteoutName::Opaque);
    }
    if name.starts_with(AUFS_META_PREFIX) {
        return Ok(WhiteoutName::Meta);
    }
    let hidden = OsStr::from_bytes(&name[WHITEOUT_PREFIX.len()..]);
    if hidden.is_empty() || hidden == "." || hidden == ".." {
        return Err(Error::Entry {
            path: raw.to_owned(),
            reason: "a whiteout must name an entry".to_owned(),
        });
    }
    Ok(WhiteoutName::Hides(hidden.to_owned()))
}

impl Extractor<'_> {
    fn run(&mut self, tar: impl Read) -> Result<()> {
        for_each_entry(tar, |entry| {
            self.stats.entries += 1;
            self.entry(entry)
        })?;
        self.finish()
    }

    fn entry<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<()> {
        let header = read_header(
            entry,
            self.wh,
            &mut self.stats.xattrs_skipped,
            &mut self.first_skipped,
        )?;
        let (raw, path, kind, meta) = match header {
            Header::Global => return Ok(()),
            Header::Whiteout { raw, path, name } => return self.whiteout(&path, name, &raw),
            Header::Entry {
                raw,
                path,
                kind,
                meta,
            } => (raw, path, kind, meta),
        };
        let refuse = |reason: &str| Error::Entry {
            path: raw.clone(),
            reason: reason.to_owned(),
        };

        // kaniko creates no device node or FIFO in its root (ADR-0041); a
        // user namespace can make no device node (ADR-0106).
        let skipped = match &kind {
            Kind::Node(RawFileType::CharacterDevice | RawFileType::BlockDevice, ..) => {
                self.live.is_some()
                    || (self.devices == DeviceNodes::Skip && !path.as_os_str().is_empty())
            }
            Kind::Node(..) => self.live.is_some(),
            Kind::Dir | Kind::File | Kind::Symlink(_) | Kind::Hardlink(_) => false,
        };
        if skipped {
            self.stats.nodes_skipped += 1;
            if self.live.is_none() {
                self.remember_skipped(path);
            }
            return Ok(());
        }
        if let Some(live) = &self.live
            && (live.keep.covers(&path)
                || matches!(&kind, Kind::Hardlink(t) if live.keep.covers(t)))
        {
            self.stats.ignored += 1;
            return Ok(());
        }
        // In a live root, the hardlink target's directory as checked, which
        // the link is then made from.
        let mut checked_target = TargetDir::Unchecked;
        if let Kind::Hardlink(t) = &kind
            && let Some(live) = &self.live
        {
            match self.live_target(live, t, &raw)? {
                Some(dir) => checked_target = dir,
                None => {
                    self.stats.ignored += 1;
                    return Ok(());
                }
            }
        }

        if path.as_os_str().is_empty() {
            // The layer's root directory: its metadata applies to the root.
            return match kind {
                Kind::Dir => self.dir_meta(&path, None, meta),
                Kind::File | Kind::Symlink(_) | Kind::Hardlink(_) | Kind::Node(..) => {
                    Err(refuse(ROOT_NOT_DIR))
                }
            };
        }
        // As root this links the device: nothing at its name now, such as a
        // lower layer's file, is what it names.
        if let Kind::Hardlink(t) = &kind
            && self.devices_skipped.contains(t)
        {
            self.stats.nodes_skipped += 1;
            self.remember_skipped(path);
            return Ok(());
        }
        // By the cleaned name, as recorded: the entry takes the place of any
        // device left out there, and of those below it unless it is a
        // directory, merged.
        self.forget_skipped(&path, !matches!(kind, Kind::Dir));
        let (parent, name) = path::entry(&path)?;
        let name = name.to_owned();
        // In a live root, `path` becomes where the entry lands on disk.
        let Some((pfd, path)) = self.parent_dir(&parent, &name, &raw)? else {
            self.stats.ignored += 1;
            return Ok(());
        };
        let is_dir = matches!(kind, Kind::Dir);
        let existing_dir = match self.make_room(&path, pfd.as_fd(), &name, is_dir, &raw)? {
            Room::Free => false,
            Room::Merge => true,
            Room::Skip => {
                self.stats.ignored += 1;
                return Ok(());
            }
        };
        if matches!(kind, Kind::Symlink(_) | Kind::Hardlink(_))
            && let Some(live) = &mut self.live
        {
            // A new symlink (a hardlink may link one) can move where a
            // cached parent lands.
            live.last = None;
        }

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
                self.stats.bytes_written += n;
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
                self.hardlink(&target, checked_target, pfd.as_fd(), &name, &path, &raw)?;
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
        self.mark_created(&path);
        Ok(())
    }

    /// Remembers `path` as a device left out, within [`SKIPPED_NAMES_MAX`]
    /// bytes a layer, counted from the names alone so that what is
    /// remembered depends on the layer only: once a name passes them,
    /// nothing more is remembered, with one warning, and a link to one of
    /// those is linked to what the layer has at that name, else copied from
    /// the nearest lower layer that shows it, else refused (ADR-0128), so
    /// that memory never grows with names the layer puts nowhere.
    fn remember_skipped(&mut self, mut path: PathBuf) {
        let cost = path.as_os_str().len().saturating_add(SKIPPED_NAME_COST);
        let total = self.skipped_bytes.saturating_add(cost);
        if total <= SKIPPED_NAMES_MAX {
            self.skipped_bytes = total;
            path.shrink_to_fit();
            self.devices_skipped.insert(path);
        } else if self.skipped_bytes <= SKIPPED_NAMES_MAX {
            self.skipped_bytes = total;
            tracing::warn!(
                "the names this layer leaves out (devices and links to them) passed the \
                 {SKIPPED_NAMES_MAX}-byte budget: a link to one past it is linked to what the \
                 layer has at that name, else copied from the nearest lower layer that shows \
                 it, else refused"
            );
        }
    }

    /// Forgets the devices left out at `path`, and below it when `below`:
    /// the layer puts something else there.
    fn forget_skipped(&mut self, path: &Path, below: bool) {
        if self.devices_skipped.is_empty() {
            return;
        }
        if !below {
            self.devices_skipped.remove(path);
            return;
        }
        let gone: Vec<PathBuf> = self
            .devices_skipped
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .take_while(|p| p.starts_with(path))
            .cloned()
            .collect();
        for p in gone {
            self.devices_skipped.remove(&p);
        }
    }

    /// The hardlink `name` in `pfd` (at `path`) to `target`: linked within
    /// this layer; else, in [`extract_overlay`], to an earlier copy of the
    /// same target, or copied from the nearest lower layer that shows it.
    /// `checked` is, in a live root, the target's directory as
    /// [`Extractor::live_target`] checked it: the link is made from that fd,
    /// never from a new resolution.
    fn hardlink(
        &mut self,
        target: &Path,
        checked: TargetDir,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        path: &Path,
        raw: &Path,
    ) -> Result<()> {
        let missing = |why: &str| Error::Entry {
            path: raw.to_owned(),
            reason: format!("the hardlink target {} {why}", target.display()),
        };
        let (tparent, tname) = path::entry(target)?;
        let tpfd = match checked {
            TargetDir::Checked(fd) => Some(fd),
            TargetDir::Missing => None,
            // Never in a live root, where `entry` checks every hardlink
            // with `live_target` first: refused rather than resolved anew.
            TargetDir::Unchecked if self.live.is_some() => {
                return Err(missing("was not checked in a live root"));
            }
            TargetDir::Unchecked => match self.root.resolve_dir_in(&tparent, Mounts::Stay) {
                Ok(fd) => Some(fd),
                Err(e) if is_noent(&e) => None,
                Err(e) => return Err(on_mount(e, raw)),
            },
        };
        if let Some(tpfd) = &tpfd {
            // A whiteout hides the name; linking it would link the device.
            if self.wh == Whiteouts::Overlay
                && meta::statx_at(tpfd.as_fd(), tname, target).is_ok_and(|st| is_whiteout(&st))
            {
                return Err(missing("is whited out in this layer"));
            }
            // Flags 0: a symlink target is linked itself, not followed.
            match rustix::fs::linkat(tpfd, tname, pfd, name, AtFlags::empty()) {
                Ok(()) => {
                    self.stats.hardlinks += 1;
                    return Ok(());
                }
                Err(Errno::NOENT) => {}
                Err(e) => return Err(io_err("link", path, e)),
            }
        }
        if self.lowers.is_empty() {
            return Err(missing("is neither in this layer nor in the root"));
        }
        let (dirs, last) = components(target);
        let hidden_here = match walk_layer(self.root, &dirs, target)? {
            Walk::Absent => false,
            Walk::Hidden => true,
            Walk::Dir { opaque, .. } => opaque,
        };
        if hidden_here {
            return Err(missing("is hidden by this layer"));
        }
        let Some((layer, lpfd)) = self.find_lower(&dirs, last, target)? else {
            return Err(missing("is in no layer below, or hidden there"));
        };
        let pin = root::open_name(
            lpfd.as_fd(),
            last,
            OFlags::PATH | OFlags::NOFOLLOW,
            0,
            true,
            target,
        )?;
        // Taken before the content is read, which may move the atime.
        let st = meta::statx_at(pin.as_fd(), OsStr::new(""), target)?;
        if st.file_type() == FileType::Directory {
            return Err(missing("is a directory"));
        }
        // One copy per lower inode, whichever of its names a link targets:
        // keyed by path, a layer linking each of a lower file's many names
        // would copy its content once per name.
        let key = (layer, st.dev_major, st.dev_minor, st.ino);
        if let Some((copy, copy_st)) = self.copied.get(&key) {
            let (cparent, cname) = path::entry(copy)?;
            // A copy the layer whited out or replaced since is forgotten
            // ([`Extractor::forget_copies`]); one it removed by another name
            // (through a symlink) is found here: its directory gone, or
            // something else at its name. Then the lower file is copied
            // again rather than what is there now linked, or the extraction
            // failed.
            let cpfd = match self.root.resolve_dir_in(&cparent, Mounts::Stay) {
                Ok(fd) => Some(fd),
                Err(e) if is_gone(&e) => None,
                Err(e) => return Err(on_mount(e, raw)),
            };
            if let Some(cpfd) = cpfd {
                let still = match meta::statx_at(cpfd.as_fd(), cname, copy) {
                    Ok(now) => is_same_copy(&now, copy_st),
                    Err(e) if is_gone(&e) => false,
                    Err(e) => return Err(e),
                };
                if still {
                    rustix::fs::linkat(&cpfd, cname, pfd, name, AtFlags::empty())
                        .map_err(|e| io_err("link", path, e))?;
                    self.stats.hardlinks += 1;
                    return Ok(());
                }
            }
        }
        let m = Meta {
            uid: Some(st.uid),
            gid: Some(st.gid),
            mode: Some(st.permissions()),
            atime: Some(st.atime),
            mtime: Some(st.mtime),
            xattrs: Vec::new(),
        };
        let sparent = Some((lpfd.as_fd(), last));
        self.stats.bytes_written += copy::copy_node(
            pin.as_fd(),
            &st,
            sparent,
            Xattrs::Layer,
            None,
            pfd,
            name,
            m,
            path,
        )?;
        self.stats.hardlinks_copied += 1;
        let copy_st = meta::statx_at(pfd, name, path)?;
        self.copied.insert(key, (path.to_owned(), copy_st));
        self.copy_names.insert(path.to_owned(), key);
        Ok(())
    }

    /// The index in `lowers` and the directory holding `target` (`dirs`
    /// then `last`) of the nearest lower layer that has it, as overlay's
    /// merged view finds it; `None` when no layer has it or a higher one
    /// hides it.
    fn find_lower(
        &self,
        dirs: &[&OsStr],
        last: &OsStr,
        target: &Path,
    ) -> Result<Option<(usize, OwnedFd)>> {
        for (i, lower) in self.lowers.iter().enumerate() {
            let (fd, opaque) = match walk_layer(lower, dirs, target)? {
                Walk::Absent => continue,
                Walk::Hidden => return Ok(None),
                Walk::Dir { fd, opaque } => (fd, opaque),
            };
            match meta::statx_at(fd.as_fd(), last, target) {
                Ok(st) if is_whiteout(&st) => return Ok(None),
                Ok(_) => return Ok(Some((i, fd))),
                Err(e) if is_noent(&e) && !opaque => {}
                Err(e) if is_noent(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        Ok(None)
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

    /// Clears the name `name` in `pfd` for a new entry at `path`. A mount
    /// point is never replaced or merged into: refused, or skipped in a
    /// live root, as is a directory holding an ignored path or a mount.
    fn make_room(
        &mut self,
        path: &Path,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        dir: bool,
        raw: &Path,
    ) -> Result<Room> {
        let st = match meta::statx_at(pfd, name, path) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                return Ok(Room::Free);
            }
            Err(e) => return Err(e),
        };
        if self.root.off_mount(&st) {
            return match self.live {
                Some(_) => Ok(Room::Skip),
                None => Err(crosses_mount(raw)),
            };
        }
        let is_dir = st.file_type() == FileType::Directory;
        if is_dir && dir {
            return Ok(Room::Merge);
        }
        if let Some(live) = &mut self.live {
            live.last = None;
        }
        if is_dir {
            if !self.remove_dir_tree(path, pfd, name, &st)? {
                return Ok(Room::Skip);
            }
        } else {
            rustix::fs::unlinkat(pfd, name, AtFlags::empty())
                .map_err(|e| io_err("unlink", path, e))?;
            self.forget_copies(path);
        }
        Ok(Room::Free)
    }

    /// Removes the directory `name` in `pfd` (at `path`, as `st` described
    /// it) and everything in it, through the deletion walker started from
    /// that very directory; forgets its pending metadata. In a live root,
    /// ignored paths and mounts below it stay, and so does the directory:
    /// returns whether it was removed.
    fn remove_dir_tree(
        &mut self,
        path: &Path,
        pfd: BorrowedFd<'_>,
        name: &OsStr,
        st: &Stat,
    ) -> Result<bool> {
        let mut counts = Counts::default();
        let start = delete::open_dir_checked(pfd, name, st, path)?;
        let keep = self.live.as_ref().map(|l| l.keep);
        let mut decide = |p: &Path, _: FileType| match keep {
            Some(keep) if keep.covers(p) => Keep::Whole,
            Some(_) | None => Keep::Remove,
        };
        let all = delete::remove_below(self.root, start, path, &mut decide, &mut counts)?;
        if !all {
            self.stats.ignored += counts.kept;
            // What is left below it is only what the deletion kept: ignored
            // paths, which this layer never wrote, and the directories
            // leading to them. Pending metadata goes with the directories
            // the walk removed, as found on disk: the ignore list only says
            // what might have been kept.
            self.created.clear_below(path);
            self.forget_copies(path);
            for p in self.pending_below(path) {
                if p == path {
                    continue;
                }
                match self.root.statx(&p) {
                    Err(e)
                        if matches!(
                            e.raw_os_error().map(Errno::from_raw_os_error),
                            Some(Errno::NOENT | Errno::NOTDIR)
                        ) =>
                    {
                        self.dirs.remove(&p);
                    }
                    // Still there, or unknown: `finish` checks it.
                    Ok(_) | Err(_) => {}
                }
            }
            return match self.live {
                Some(_) => Ok(false),
                None => Err(Error::Entry {
                    path: path.to_owned(),
                    reason: "replaces a directory holding a mount point".to_owned(),
                }),
            };
        }
        rustix::fs::unlinkat(pfd, name, AtFlags::REMOVEDIR)
            .map_err(|e| io_err("rmdir", path, e))?;
        self.created.remove(path);
        self.forget_copies(path);
        for p in self.pending_below(path) {
            self.dirs.remove(&p);
        }
        Ok(true)
    }

    /// Forgets the hardlink copies at or below `path`, which the layer just
    /// removed: a later link to their lower inode copies it again.
    fn forget_copies(&mut self, path: &Path) {
        let gone: Vec<PathBuf> = self
            .copy_names
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .map(|(p, _)| p)
            .take_while(|p| p.starts_with(path))
            .cloned()
            .collect();
        for p in gone {
            if let Some(key) = self.copy_names.remove(&p)
                && self.copied.get(&key).is_some_and(|(at, _)| *at == p)
            {
                self.copied.remove(&key);
            }
        }
    }

    /// The directories with pending metadata at or below `path`: one range
    /// of `dirs`, as paths sort component by component.
    fn pending_below(&self, path: &Path) -> Vec<PathBuf> {
        self.dirs
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .map(|(p, _)| p)
            .take_while(|p| p.starts_with(path))
            .cloned()
            .collect()
    }

    fn whiteout(&mut self, path: &Path, name: WhiteoutName, raw: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new("")).to_owned();
        let hidden = match name {
            WhiteoutName::Meta => return Ok(()),
            WhiteoutName::Hides(hidden) => hidden,
            WhiteoutName::Opaque => {
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
                    Whiteouts::Literal => Err(literal_whiteout(raw)),
                };
            }
        };
        let hidden = hidden.as_os_str();
        let target = dir.join(hidden);
        self.stats.whiteouts += 1;
        match self.wh {
            Whiteouts::Overlay => {
                let pfd = self
                    .root
                    .mkdir_all(&dir, PARENT_MODE, Mounts::Stay)
                    .map_err(|e| on_mount(e, raw))?;
                self.make_room(&target, pfd.as_fd(), hidden, false, raw)?;
                self.forget_skipped(&target, true);
                rustix::fs::mknodat(
                    &pfd,
                    hidden,
                    RawFileType::CharacterDevice,
                    Mode::empty(),
                    rustix::fs::makedev(0, 0),
                )
                .map_err(|e| io_err("mknod whiteout", &target, e))
            }
            Whiteouts::Literal => Err(literal_whiteout(raw)),
            Whiteouts::Apply => {
                if let Some(live) = &mut self.live {
                    live.last = None;
                    if live.keep.covers(&target) || live.keep.has_below(&target) {
                        self.stats.ignored += 1;
                        return Ok(());
                    }
                }
                let pfd = match self.root.resolve_dir_in(&dir, Mounts::Stay) {
                    Ok(fd) => fd,
                    // Nothing below a missing directory to hide.
                    Err(e) if is_noent(&e) => return Ok(()),
                    Err(e) if self.live.is_some() && is_xdev(&e) => {
                        self.stats.ignored += 1;
                        return Ok(());
                    }
                    Err(e) => return Err(on_mount(e, raw)),
                };
                let target = match &self.live {
                    Some(live) => {
                        let t = live.physical(pfd.as_fd(), &dir)?.join(hidden);
                        if live.keep.covers(&t) || live.keep.has_below(&t) {
                            self.stats.ignored += 1;
                            return Ok(());
                        }
                        t
                    }
                    None => target,
                };
                match meta::statx_at(pfd.as_fd(), hidden, &target) {
                    Err(e) if is_noent(&e) => Ok(()),
                    Err(e) => Err(e),
                    Ok(st) if self.root.off_mount(&st) => match self.live {
                        Some(_) => {
                            self.stats.ignored += 1;
                            Ok(())
                        }
                        None => Err(crosses_mount(raw)),
                    },
                    Ok(st) if st.file_type() == FileType::Directory => self
                        .remove_dir_tree(&target, pfd.as_fd(), hidden, &st)
                        .map(drop),
                    Ok(_) => {
                        rustix::fs::unlinkat(&pfd, hidden, AtFlags::empty())
                            .map_err(|e| io_err("unlink", &target, e))?;
                        self.created.remove(&target);
                        self.forget_copies(&target);
                        Ok(())
                    }
                }
            }
        }
    }

    /// Empties `dir` of everything this layer did not create (nor, in a
    /// live root, what is ignored or on another mount).
    fn apply_opaque(&mut self, dir: &Path, raw: &Path) -> Result<()> {
        if let Some(live) = &mut self.live {
            live.last = None;
            if live.keep.covers(dir) {
                self.stats.ignored += 1;
                return Ok(());
            }
        }
        let start = match delete::open_dir(self.root, dir) {
            Ok(fd) => fd,
            // A directory missing from the root hides nothing. Only the
            // start may be missing: an `ENOENT` during the walk is an error.
            Err(e) if is_noent(&e) => return Ok(()),
            Err(e) if self.live.is_some() && is_xdev(&e) => {
                self.stats.ignored += 1;
                return Ok(());
            }
            Err(e) => return Err(on_mount(e, raw)),
        };
        let physical;
        let (dir, keep) = match &self.live {
            Some(live) => {
                physical = live.physical(start.as_fd(), dir)?;
                if live.keep.covers(&physical) {
                    self.stats.ignored += 1;
                    return Ok(());
                }
                (physical.as_path(), Some(live.keep))
            }
            None => (dir, None),
        };
        let created = &self.created;
        let mut decide = |p: &Path, t: FileType| {
            if let Some(keep) = keep {
                if keep.covers(p) {
                    return Keep::Whole;
                }
                if keep.has_below(p) {
                    return Keep::Descend;
                }
            }
            match created.lookup(p) {
                Mark::Created if t == FileType::Directory => Keep::Descend,
                Mark::Created => Keep::Whole,
                Mark::Ancestor => Keep::Descend,
                Mark::None => Keep::Remove,
            }
        };
        // Pending directory metadata needs no cleanup: it belongs to
        // directories of this layer, which are all kept.
        let mut counts = Counts::default();
        delete::remove_below(self.root, start, dir, &mut decide, &mut counts).map(drop)
    }

    /// The parent directory `parent` of a new entry `name`, created if
    /// missing, and the path the entry gets inside the root: `parent/name`,
    /// or in a live root its physical path. `None` in a live root when the
    /// entry is ignored there or the parent is on another mount.
    ///
    /// In a live root the fd returned is the one whose physical path was
    /// checked: the deepest existing directory of `parent`, then each
    /// missing name created and opened relative to it, without following a
    /// symlink or entering another mount. The entry is written through that
    /// fd, so a later swap of a name in `parent` cannot redirect it.
    fn parent_dir(
        &mut self,
        parent: &Path,
        name: &OsStr,
        raw: &Path,
    ) -> Result<Option<(OwnedFd, PathBuf)>> {
        let Some(live) = &mut self.live else {
            let pfd = self
                .root
                .mkdir_all(parent, PARENT_MODE, Mounts::Stay)
                .map_err(|e| on_mount(e, raw))?;
            return Ok(Some((pfd, parent.join(name))));
        };
        if let Some((lexical, fd, physical)) = &live.last
            && lexical == parent
        {
            let path = physical.join(name);
            if live.keep.covers(&path) {
                return Ok(None);
            }
            return Ok(Some((dup(fd.as_fd(), &path)?, path)));
        }
        // The deepest directory of `parent` that exists, then the names
        // still to create below it.
        let mut existing = parent.to_owned();
        let mut missing = Vec::new();
        let mut cur = loop {
            match self.root.resolve_dir_in(&existing, Mounts::Stay) {
                Ok(fd) => break fd,
                Err(e) if is_noent(&e) => {
                    let Some(n) = existing.file_name() else {
                        return Err(e);
                    };
                    missing.push(n.to_owned());
                    existing.pop();
                }
                Err(e) if is_xdev(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        };
        let mut physical = live.physical(cur.as_fd(), &existing)?;
        missing.reverse();
        let full: PathBuf = physical
            .iter()
            .chain(missing.iter().map(|n| n.as_os_str()))
            .collect();
        if live.keep.covers(full.join(name)) {
            return Ok(None);
        }
        let mode = Mode::from_raw_mode(PARENT_MODE);
        for n in &missing {
            physical.push(n);
            match rustix::fs::mkdirat(&cur, n.as_os_str(), mode) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(io_err("mkdir", &physical, e)),
            }
            cur = match root::open_name(
                cur.as_fd(),
                n,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                0,
                true,
                &physical,
            ) {
                Ok(fd) => fd,
                Err(e) if is_xdev(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
        }
        let path = physical.join(name);
        live.last = Some((parent.to_owned(), dup(cur.as_fd(), &physical)?, physical));
        Ok(Some((cur, path)))
    }

    /// In the live root `live`, the directory of the hardlink target
    /// `target`, as checked: `None` when the target is ignored at its
    /// physical path or on another mount, so the hardlink is skipped;
    /// [`TargetDir::Missing`] when its directory is missing, left for
    /// [`Extractor::hardlink`] to report.
    fn live_target(&self, live: &Live<'_>, target: &Path, raw: &Path) -> Result<Option<TargetDir>> {
        let (tparent, tname) = path::entry(target)?;
        let fd = match self.root.resolve_dir_in(&tparent, Mounts::Stay) {
            Ok(fd) => fd,
            Err(e) if is_noent(&e) => return Ok(Some(TargetDir::Missing)),
            Err(e) if is_xdev(&e) => return Ok(None),
            Err(e) => return Err(on_mount(e, raw)),
        };
        let t = live.physical(fd.as_fd(), &tparent)?.join(tname);
        if live.keep.covers(&t) {
            return Ok(None);
        }
        match meta::statx_at(fd.as_fd(), tname, &t) {
            Ok(st) if self.root.off_mount(&st) => Ok(None),
            Ok(_) => Ok(Some(TargetDir::Checked(fd))),
            Err(e) if is_noent(&e) => Ok(Some(TargetDir::Checked(fd))),
            Err(e) => Err(e),
        }
    }

    /// Records `path` as created by this layer, for opaque whiteouts.
    fn mark_created(&mut self, path: &Path) {
        if self.wh == Whiteouts::Apply {
            self.created.insert(path);
        }
    }

    /// Directory modes and times, deepest first.
    fn finish(&mut self) -> Result<()> {
        for (path, m) in self.dirs.iter().rev() {
            // A pending directory that a later entry replaced was dropped
            // from the map; the type check on the pinned entry only guards
            // against a change made outside the extraction.
            let Some(live) = &self.live else {
                self.root.set_dir_meta(path, m)?;
                continue;
            };
            // In a live root `path` is physical: the parent reached by it
            // must still be there, not somewhere a swap moved it to.
            let Some((parent, name)) = path.parent().zip(path.file_name()) else {
                self.root.set_dir_meta(path, m)?;
                continue;
            };
            let pfd = self.root.resolve_dir_in(parent, Mounts::Stay)?;
            if live.physical(pfd.as_fd(), parent)? != parent {
                return Err(Error::Changed { path: path.clone() });
            }
            root::set_meta_at(pfd.as_fd(), name, m, Some(FileType::Directory), path)?;
        }
        Ok(())
    }
}

/// A hardlink target's directory, as [`Extractor::hardlink`] gets it.
enum TargetDir {
    /// Not checked (not a live root): `hardlink` resolves it itself. In a
    /// live root every hardlink is checked first, so `hardlink` refuses it
    /// there instead of resolving it anew.
    Unchecked,
    /// Checked by [`Extractor::live_target`] and missing: left for
    /// `hardlink` to report.
    Missing,
    /// Checked by [`Extractor::live_target`]: the link is made from this
    /// fd, never from a new resolution.
    Checked(OwnedFd),
}

/// What [`Extractor::make_room`] left at a name.
enum Room {
    /// Nothing: the entry can be created.
    Free,
    /// A directory, which the new directory merges into.
    Merge,
    /// Something a live root keeps: the entry is skipped.
    Skip,
}

/// What [`extract_live`] needs besides the root.
struct Live<'r> {
    keep: &'r IgnoreList,
    /// `/proc/self/fd`, checked to be procfs.
    proc: OwnedFd,
    /// Where the root is on the host, as `/proc/self/fd` shows it.
    root_host: Vec<u8>,
    /// The last parent directory resolved: its path in the layer, its fd
    /// as checked and its physical path. Dropped whenever the layer removes something or
    /// adds a symlink, which may move it.
    last: Option<(PathBuf, OwnedFd, PathBuf)>,
}

impl<'r> Live<'r> {
    fn new(root: &RootPath, keep: &'r IgnoreList) -> Result<Self> {
        let proc = meta::proc_fd_dir(Path::new(""))?;
        let root_host = read_fd_link(proc.as_fd(), root.fd(), Path::new(""))?;
        Ok(Self {
            keep,
            proc,
            root_host,
            last: None,
        })
    }

    /// The physical path inside the root of the directory `fd` (reached
    /// as `at`), from the kernel's own name for it: `""` for the root.
    fn physical(&self, fd: BorrowedFd<'_>, at: &Path) -> Result<PathBuf> {
        let link = read_fd_link(self.proc.as_fd(), fd, at)?;
        let rel = if self.root_host == b"/" {
            link.strip_prefix(b"/")
        } else {
            link.strip_prefix(&self.root_host[..]).and_then(|r| {
                if r.is_empty() {
                    Some(r)
                } else {
                    r.strip_prefix(b"/")
                }
            })
        };
        rel.and_then(|r| path::clean(Path::new(OsStr::from_bytes(r))))
            .ok_or_else(|| Error::Entry {
                path: at.to_owned(),
                reason: "cannot tell where this directory lies in the root".to_owned(),
            })
    }
}

/// The target of `/proc/self/fd/<fd>`: the kernel's path of `fd`.
fn read_fd_link(proc: BorrowedFd<'_>, fd: BorrowedFd<'_>, at: &Path) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd as _;
    rustix::fs::readlinkat(proc, fd.as_raw_fd().to_string(), Vec::new())
        .map(std::ffi::CString::into_bytes)
        .map_err(|e| io_err("readlink /proc/self/fd", at, e))
}

/// A second `O_CLOEXEC` fd of the same open directory.
fn dup(fd: BorrowedFd<'_>, at: &Path) -> Result<OwnedFd> {
    rustix::io::fcntl_dupfd_cloexec(fd, 0).map_err(|e| io_err("dup", at, e))
}

/// The `mode` of the extraction span.
/// An error of the `tar` crate's reads: [`Error::ExtensionHeader`] when the
/// `tar-header` limit stopped it, else [`Error::Tar`].
fn tar_error(e: std::io::Error) -> Error {
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<limit::TooLarge>())
    {
        Some(too_large) => Error::ExtensionHeader {
            size: too_large.size,
            max: too_large.max,
        },
        None => Error::Tar(e),
    }
}

fn mode_name(wh: Whiteouts) -> &'static str {
    match wh {
        Whiteouts::Apply => "Apply",
        Whiteouts::Overlay => "Overlay",
        Whiteouts::Literal => "Literal",
    }
}

/// [`Whiteouts::Literal`] never reaches the whiteout code: its names are
/// files. Kept as an error rather than a panic.
fn literal_whiteout(raw: &Path) -> Error {
    Error::Entry {
        path: raw.to_owned(),
        reason: "a whiteout in a literal extraction".to_owned(),
    }
}

fn is_xdev(e: &Error) -> bool {
    e.raw_os_error() == Some(Errno::XDEV.raw_os_error())
}

/// What one layer in the overlay form shows at a directory path.
enum Walk {
    /// The layer does not have the path; lower layers may.
    Absent,
    /// A whiteout or a non-directory on the way, or a missing directory
    /// below an opaque one: lower layers are hidden.
    Hidden,
    /// The directory, `O_RDONLY`; `opaque` when it or a directory above it
    /// is opaque, so that what this layer lacks below it is hidden.
    Dir { fd: OwnedFd, opaque: bool },
}

/// Walks the directories `dirs` in the extracted layer `layer`, by name,
/// following no symlink and crossing no mount. `target` is only for
/// messages.
fn walk_layer(layer: &RootPath, dirs: &[&OsStr], target: &Path) -> Result<Walk> {
    let mut cur = layer.resolve_dir_in(Path::new(""), Mounts::Stay)?;
    let mut opaque = false;
    for d in dirs {
        match meta::statx_at(cur.as_fd(), d, target) {
            Ok(st) if st.file_type() == FileType::Directory => {}
            Ok(_) => return Ok(Walk::Hidden),
            Err(e) if is_noent(&e) && opaque => return Ok(Walk::Hidden),
            Err(e) if is_noent(&e) => return Ok(Walk::Absent),
            Err(e) => return Err(e),
        }
        cur = root::open_name(
            cur.as_fd(),
            d,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            0,
            true,
            target,
        )?;
        let mut v = [0; 2];
        opaque |= rustix::fs::fgetxattr(&cur, OVERLAY_OPAQUE, &mut v[..])
            .is_ok_and(|n| v.get(..n) == Some(&b"y"[..]));
    }
    Ok(Walk::Dir { fd: cur, opaque })
}

/// The parent directories and the last name of a cleaned, non-empty path.
fn components(p: &Path) -> (Vec<&OsStr>, &OsStr) {
    let mut names: Vec<&OsStr> = p.iter().collect();
    let last = names.pop().unwrap_or_default();
    (names, last)
}

/// An overlay whiteout: the character device 0/0.
fn is_whiteout(st: &Stat) -> bool {
    st.file_type() == FileType::CharDevice && st.rdev_major == 0 && st.rdev_minor == 0
}

fn is_noent(e: &Error) -> bool {
    e.raw_os_error() == Some(Errno::NOENT.raw_os_error())
}

/// The name, or a directory on the way to it, is no longer there.
fn is_gone(e: &Error) -> bool {
    matches!(
        e.raw_os_error().map(Errno::from_raw_os_error),
        Some(Errno::NOENT | Errno::NOTDIR)
    )
}

/// Whether `now` is still the hardlink copy `copy` described when it was
/// made: the same inode, and a regular file of the same size and mtime, so
/// neither an inode number the filesystem reused for a new file nor a
/// whiteout device passes.
fn is_same_copy(now: &Stat, copy: &Stat) -> bool {
    now.same_inode(copy)
        && now.file_type() == copy.file_type()
        && now.size == copy.size
        && now.mtime == copy.mtime
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
