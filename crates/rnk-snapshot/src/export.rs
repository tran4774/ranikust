//! The exporter: one instruction's upper directory, read against its lower
//! stack, fed to `rnk-layer`'s writer as an OCI layer
//! (`design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`, as
//! BuildKit's `WriteUpperdir`).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use rnk_layer::{
    EncodeOptions, Entry, EntryKind, EntryPath, EntrySource, LayerDigests, LayerWriter,
};
use rnk_rootfs::{FileType, OpenHow, RootPath, Stat};

use crate::error::{Error, Result};
use crate::lowers::{self, Found, Lowers, OPAQUE, REDIRECT};

/// One instruction's diff: the upper directory `diff-<k>` over the lower
/// stack it was mounted on. Holds paths only; the directories stay on disk
/// until the stage is released, so a layer can be exported later, or
/// never (lazy export).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerDiff {
    pub(crate) upper: PathBuf,
    /// Topmost first.
    pub(crate) lowers: Vec<PathBuf>,
}

/// What [`LayerDiff::export`] found, besides the layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffCounts {
    /// Entries fed to the writer other than whiteouts: files, directories,
    /// links, nodes.
    pub files_added: u64,
    /// Whiteouts written (`.wh.<name>`).
    pub whiteouts: u64,
    /// Opaque directories written (`.wh..wh..opq`).
    pub opaque_dirs: u64,
    /// Bytes of regular file content.
    pub bytes: u64,
}

impl LayerDiff {
    /// The diff of the upper directory `upper` over `lowers` (topmost
    /// first), in the overlay form with `userxattr`, as an unmounted
    /// overlay leaves them. [`StageRootfs::end_layer`] makes these; this is
    /// for diffs kept elsewhere.
    ///
    /// [`StageRootfs::end_layer`]: crate::StageRootfs::end_layer
    #[must_use]
    pub fn new(upper: PathBuf, lowers: Vec<PathBuf>) -> Self {
        Self { upper, lowers }
    }

    /// The upper directory, for messages.
    #[must_use]
    pub fn upper(&self) -> &Path {
        &self.upper
    }

    /// Whether the upper directory holds no entry at all: the instruction
    /// changed nothing on disk.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when the upper directory cannot be read.
    pub fn is_empty(&self) -> Result<bool> {
        let upper = RootPath::open(&self.upper)?;
        Ok(list(&upper, Path::new(""))?.is_empty())
    }

    /// Writes the diff as a layer to `out` and returns its digests and
    /// counts. The bytes depend only on the diff and on `options` other
    /// than its thread count.
    ///
    /// Whiteouts are converted before hardlinks are found, so overlay's
    /// whiteouts, which share one inode, never become hardlinks. A whiteout
    /// or an opaque marker is written only where a lower layer has the
    /// name; a directory copied up unchanged is not written by itself
    /// (BuildKit's `sameDirent`), but the writer still writes it as the
    /// parent of a changed entry, with its metadata from this exporter.
    /// Overlay's own xattrs are left out and escaped content xattrs
    /// restored; every other xattr is kept [open: Q-025].
    ///
    /// # Errors
    ///
    /// [`Error::Export`] for a redirect (never made with
    /// `redirect_dir=nofollow`) or an entry of unknown type;
    /// [`Error::Layer`] when encoding fails; [`Error::Rootfs`] when a
    /// directory cannot be read.
    pub fn export<W: Write>(
        &self,
        options: EncodeOptions,
        out: W,
    ) -> Result<(LayerDigests, DiffCounts)> {
        let span = tracing::info_span!(
            "rnk_snapshot.export",
            entries = tracing::field::Empty,
            bytes_read = tracing::field::Empty,
        );
        let _entered = span.enter();
        let upper = RootPath::open(&self.upper)?;
        let lower_roots = self
            .lowers
            .iter()
            .map(RootPath::open)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let lowers = Lowers {
            layers: &lower_roots,
        };
        let (entries, counts) = {
            let diff = tracing::info_span!(
                "rnk_snapshot.diff",
                files_added = tracing::field::Empty,
                whiteouts = tracing::field::Empty,
                opaque_dirs = tracing::field::Empty,
                bytes = tracing::field::Empty,
            );
            let _entered = diff.enter();
            let mut walk = Walk {
                upper: &upper,
                lowers: &lowers,
                entries: Vec::new(),
                counts: DiffCounts::default(),
            };
            walk.dir(Path::new(""))?;
            diff.record("files_added", walk.counts.files_added);
            diff.record("whiteouts", walk.counts.whiteouts);
            diff.record("opaque_dirs", walk.counts.opaque_dirs);
            diff.record("bytes", walk.counts.bytes);
            (walk.entries, walk.counts)
        };
        let mut writer = LayerWriter::new(options)?;
        for e in entries {
            writer.add(e)?;
        }
        span.record("entries", writer.len());
        let mut source = Source {
            upper: &upper,
            lowers: &lowers,
            bytes_read: 0,
        };
        let digests = writer.finish(&mut source, out)?;
        span.record("bytes_read", source.bytes_read);
        Ok((digests, counts))
    }

    /// The entries [`LayerDiff::export`] feeds the writer, sorted by path;
    /// for tests and for callers that only want to look.
    ///
    /// # Errors
    ///
    /// As [`LayerDiff::export`].
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let upper = RootPath::open(&self.upper)?;
        let lower_roots = self
            .lowers
            .iter()
            .map(RootPath::open)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let lowers = Lowers {
            layers: &lower_roots,
        };
        let mut walk = Walk {
            upper: &upper,
            lowers: &lowers,
            entries: Vec::new(),
            counts: DiffCounts::default(),
        };
        walk.dir(Path::new(""))?;
        let mut entries = walk.entries;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
}

/// The names in the directory `dir` of `root`, without `.` and `..`,
/// sorted. Opened without following a final symlink.
pub(crate) fn list(root: &RootPath, dir: &Path) -> Result<Vec<OsString>> {
    let at = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let fd = root.open_at(at, &OpenHow::read().directory().no_follow())?;
    let mut names = Vec::new();
    let io_err = |e: rustix::io::Errno| Error::Io {
        op: "read directory",
        path: dir.to_owned(),
        source: e.into(),
    };
    for item in rustix::fs::Dir::read_from(&fd).map_err(io_err)? {
        let item = item.map_err(io_err)?;
        let name = item.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    names.sort();
    Ok(names)
}

struct Walk<'a> {
    upper: &'a RootPath,
    lowers: &'a Lowers<'a>,
    entries: Vec<Entry>,
    counts: DiffCounts,
}

impl Walk<'_> {
    fn dir(&mut self, dir: &Path) -> Result<()> {
        for name in list(self.upper, dir)? {
            let p = dir.join(&name);
            let st = self.upper.statx(&p)?;
            self.entry(&p, &st)?;
        }
        Ok(())
    }

    fn entry(&mut self, p: &Path, st: &Stat) -> Result<()> {
        let path = entry_path(p)?;
        if lowers::is_whiteout(st) {
            if self.lowers.lookup(p)?.is_some() {
                self.counts.whiteouts += 1;
                self.entries.push(Entry {
                    path,
                    kind: EntryKind::Whiteout,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    mtime: st.mtime.secs,
                    xattrs: BTreeMap::new(),
                    inode: None,
                });
            }
            return Ok(());
        }
        let all = self.upper.xattrs(p)?;
        if all
            .iter()
            .any(|(n, _)| n == REDIRECT || n == "trusted.overlay.redirect")
        {
            return Err(Error::Export {
                path: p.to_owned(),
                reason: "an overlay redirect, which redirect_dir=nofollow never creates",
            });
        }
        let opaque = all.iter().any(|(n, v)| n == OPAQUE && v.as_slice() == b"y");
        let xattrs = lowers::content_xattrs(all);
        let kind = match st.file_type() {
            FileType::Directory => {
                let lower = self.lowers.lookup(p)?;
                let lower_dir = lower
                    .as_ref()
                    .is_some_and(|f| f.stat.file_type() == FileType::Directory);
                if opaque && lower_dir {
                    self.counts.opaque_dirs += 1;
                    EntryKind::OpaqueDir
                } else if !opaque && lower_dir && same_dir(st, &xattrs, lower.as_ref(), p)? {
                    // Copied up unchanged: not a change by itself.
                    return self.dir(p);
                } else {
                    EntryKind::Dir
                }
            }
            FileType::Regular => {
                self.counts.bytes += st.size;
                EntryKind::File { size: st.size }
            }
            FileType::Symlink => {
                EntryKind::Symlink(self.upper.read_link(p)?.into_os_string().into_vec())
            }
            FileType::CharDevice => EntryKind::Char(st.rdev_major, st.rdev_minor),
            FileType::BlockDevice => EntryKind::Block(st.rdev_major, st.rdev_minor),
            FileType::Fifo => EntryKind::Fifo,
            // A socket is no layer content (as kaniko and BuildKit).
            FileType::Socket => return Ok(()),
            FileType::Unknown => {
                return Err(Error::Export {
                    path: p.to_owned(),
                    reason: "an entry of unknown type",
                });
            }
        };
        let is_dir = kind.is_dir();
        self.counts.files_added += 1;
        self.entries.push(Entry {
            path,
            inode: (!is_dir && st.nlink > 1).then_some((dev(st), st.ino)),
            kind,
            mode: st.mode,
            uid: u64::from(st.uid),
            gid: u64::from(st.gid),
            mtime: st.mtime.secs,
            xattrs: xattrs.into_iter().map(|(n, v)| (n.into_vec(), v)).collect(),
        });
        if is_dir { self.dir(p) } else { Ok(()) }
    }
}

/// Whether the upper directory `st` with content xattrs `xattrs` equals the
/// lower directory `lower` (BuildKit's `sameDirent`): mode, owner,
/// modification time to the nanosecond and content xattrs.
fn same_dir(
    st: &Stat,
    xattrs: &[(OsString, Vec<u8>)],
    lower: Option<&Found<'_>>,
    p: &Path,
) -> Result<bool> {
    let Some(lower) = lower else {
        return Ok(false);
    };
    let l = &lower.stat;
    if (st.mode, st.uid, st.gid, st.mtime) != (l.mode, l.uid, l.gid, l.mtime) {
        return Ok(false);
    }
    Ok(lowers::content_xattrs(lower.layer.xattrs(p)?) == xattrs)
}

fn dev(st: &Stat) -> u64 {
    (u64::from(st.dev_major) << 32) | u64::from(st.dev_minor)
}

fn entry_path(p: &Path) -> Result<EntryPath> {
    Ok(EntryPath::new(p.as_os_str().as_bytes())?)
}

/// File contents and parent directories for the writer: the upper
/// directory first, which holds every parent of a changed entry (overlay
/// copies parents up), then the lower stack.
struct Source<'a> {
    upper: &'a RootPath,
    lowers: &'a Lowers<'a>,
    bytes_read: u64,
}

impl EntrySource for Source<'_> {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let p = Path::new(OsStr::from_bytes(entry.path.as_bytes()));
        let fd = self.upper.open_regular(p).map_err(io::Error::other)?;
        if let EntryKind::File { size } = entry.kind {
            self.bytes_read += size;
        }
        Ok(Box::new(File::from(fd)))
    }

    fn parent(&mut self, path: &EntryPath) -> io::Result<Option<Entry>> {
        let p = Path::new(OsStr::from_bytes(path.as_bytes()));
        let (st, xattrs) = match self.upper.statx(p) {
            Ok(st) => (st, self.upper.xattrs(p).map_err(io::Error::other)?),
            Err(_) => match self.lowers.lookup(p).map_err(io::Error::other)? {
                Some(f) => (f.stat, f.layer.xattrs(p).map_err(io::Error::other)?),
                None => return Ok(None),
            },
        };
        if st.file_type() != FileType::Directory {
            return Ok(None);
        }
        Ok(Some(Entry {
            path: path.clone(),
            kind: EntryKind::Dir,
            mode: st.mode,
            uid: u64::from(st.uid),
            gid: u64::from(st.gid),
            mtime: st.mtime.secs,
            xattrs: lowers::content_xattrs(xattrs)
                .into_iter()
                .map(|(n, v)| (n.into_vec(), v))
                .collect(),
            inode: None,
        }))
    }
}

#[cfg(test)]
mod tests;
