//! The kaniko-mode exporter: a [`WalkDiff`] of a live root fed to
//! `rnk-layer`'s writer as an OCI layer, as kaniko's `writeToTar`
//! (`kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:341-391`;
//! `design/kaniko-mode.md#exporter-requirements-specific-to-the-walk`).
//!
//! - Each changed entry is written with the metadata the walk saw; a
//!   directory unchanged itself but above a written entry or whiteout comes
//!   back from the writer as a parent, with its metadata read from the root
//!   now, as kaniko's `addParentDirectories` (`:432-447`) adds every parent
//!   but the root with `AddFileToTar`. The root itself is never written
//!   (`:348`, `addedPaths[config.RootDir] = true`).
//! - Each deleted path is one whiteout, the walk listing only the topmost;
//!   it has mode 0, owner 0:0 and time 0, as kaniko's `Tar.Whiteout`
//!   (`pkg/util/tar_util.go:162-176`) and the overlay exporter write it.
//! - Entries of the layer sharing an inode become hardlinks to the first in
//!   layer order; a name whose other links are not in the layer, such as
//!   `ln /etc/passwd /x` against a lower layer, is a regular file, since
//!   the walk does not report a link-count-only change (`ADR-0012`).
//! - Of the xattrs, only `security.capability` is kept, as the overlay
//!   exporter (ADR-0032); sockets are left out, as kaniko
//!   (`pkg/util/tar_util.go:74-77`) and BuildKit.
//! - A diff with nothing in it is still a layer: the empty tar kaniko's
//!   `TakeSnapshotFS` writes for a `RUN` that changes nothing.
//!
//! The root is live, so every entry is pinned once with `RootPath::pin`:
//! its physical path resolved with no symlink in any component and without
//! leaving the root's mount, the `O_PATH` descriptor's `statx` compared
//! with the walk's record (type, device, inode). The link target, the
//! xattrs and a file's content are read from that pin, the content through
//! a reopen of exactly that inode. A FIFO or a device node is never opened,
//! no symlink is followed, and an entry changed since the walk fails the
//! export. Names the walk saw as one inode that disagree on what they are
//! (an inode number reused during the walk) are not linked.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use rnk_layer::{
    EncodeOptions, Entry, EntryKind, EntryPath, EntrySource, LayerDigests, LayerWriter,
};
use rnk_rootfs::{FileType, Pinned, RootPath, Stat};
use xxhash_rust::xxh3::Xxh3;

use super::walk::{EntryMeta, WalkDiff};
use crate::error::{Error, Result};
use crate::export::{DiffCounts, entry_path};
use crate::lowers;

impl WalkDiff {
    /// Whether the walk found nothing added, changed or deleted: the
    /// instruction changed nothing on disk. Its layer is still written, as
    /// an empty tar.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty()
    }

    /// Writes the diff as a layer to `out`, reading the entries from `root`,
    /// the root the diff was walked on, and returns the layer's digests and
    /// counts. The bytes depend only on the diff, the root's content and
    /// `options` other than its thread count; never on the walk's worker
    /// count.
    ///
    /// # Errors
    ///
    /// [`Error::Export`] for an entry that changed since the walk (another
    /// inode or type at its path) or of unknown type; [`Error::Layer`]
    /// when encoding fails, a file's size changed since the walk included;
    /// [`Error::Rootfs`] when an entry cannot be read.
    pub fn export<W: Write>(
        &self,
        root: &RootPath,
        options: EncodeOptions,
        out: W,
    ) -> Result<(LayerDigests, DiffCounts)> {
        let span = tracing::info_span!(
            "rnk_snapshot.export",
            entries = tracing::field::Empty,
            bytes_read = tracing::field::Empty,
        );
        let _entered = span.enter();
        let (entries, counts) = {
            let diff = tracing::info_span!(
                "rnk_snapshot.diff",
                files_added = tracing::field::Empty,
                whiteouts = tracing::field::Empty,
                opaque_dirs = 0_u64,
                bytes = tracing::field::Empty,
                unchanged = 0_u64,
                bytes_compared = 0_u64,
            );
            let _entered = diff.enter();
            let (entries, counts) = self.collect(root)?;
            diff.record("files_added", counts.files_added);
            diff.record("whiteouts", counts.whiteouts);
            diff.record("bytes", counts.bytes);
            (entries, counts)
        };
        let mut writer = LayerWriter::new(options)?;
        for e in entries {
            writer.add(e)?;
        }
        span.record("entries", writer.len());
        let mut source = Source {
            root,
            files: self
                .changed
                .iter()
                .filter(|c| c.meta.kind == FileType::Regular)
                .map(|c| (c.path.as_path(), c.meta))
                .collect(),
            bytes_read: 0,
        };
        let digests = writer.finish(&mut source, out)?;
        span.record("bytes_read", source.bytes_read);
        Ok((digests, counts))
    }

    /// The entries [`WalkDiff::export`] feeds the writer, sorted by path,
    /// without the parents the writer adds; for tests and for callers that
    /// only want to look.
    ///
    /// # Errors
    ///
    /// As [`WalkDiff::export`].
    pub fn entries(&self, root: &RootPath) -> Result<Vec<Entry>> {
        let (mut entries, _) = self.collect(root)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// The layer entries of the diff and their counts: whiteouts first,
    /// then the changed entries, in the diff's order.
    fn collect(&self, root: &RootPath) -> Result<(Vec<Entry>, DiffCounts)> {
        let mut counts = DiffCounts::default();
        let mut entries = Vec::with_capacity(self.deleted.len() + self.changed.len());
        for p in &self.deleted {
            counts.whiteouts += 1;
            entries.push(Entry {
                path: entry_path(p)?,
                kind: EntryKind::Whiteout,
                mode: 0,
                uid: 0,
                gid: 0,
                mtime: 0,
                xattrs: BTreeMap::new(),
                inode: None,
            });
        }
        for c in &self.changed {
            let Some(e) = make_entry(root, &c.path, &c.meta)? else {
                continue;
            };
            if let EntryKind::File { size } = e.kind {
                counts.bytes += size;
            }
            counts.files_added += 1;
            entries.push(e);
        }
        split_reused_inodes(&mut entries);
        Ok((entries, counts))
    }
}

/// The layer entry for `p`, which the walk saw as `meta`; `None` for a
/// socket. The entry is pinned once ([`RootPath::pin`]: no symlink in any
/// component, on the root's mount) and checked to be the walk's inode;
/// its link target and xattrs are read from that pin.
fn make_entry(root: &RootPath, p: &Path, meta: &EntryMeta) -> Result<Option<Entry>> {
    if meta.kind == FileType::Socket {
        return Ok(None);
    }
    let pinned = pin_same(root, p, meta)?;
    let kind = match meta.kind {
        FileType::Directory => EntryKind::Dir,
        FileType::Regular => EntryKind::File { size: meta.size },
        FileType::Symlink => EntryKind::Symlink(pinned.read_link()?.into_os_string().into_vec()),
        FileType::CharDevice => EntryKind::Char(meta.rdev.0, meta.rdev.1),
        FileType::BlockDevice => EntryKind::Block(meta.rdev.0, meta.rdev.1),
        FileType::Fifo => EntryKind::Fifo,
        FileType::Socket => return Ok(None),
        FileType::Unknown => {
            return Err(Error::Export {
                path: p.to_owned(),
                reason: "an entry of unknown type",
            });
        }
    };
    let is_dir = kind.is_dir();
    // Only files and directories carry xattrs the walk compares; of those,
    // only `security.capability` reaches the layer.
    let xattrs = if matches!(meta.kind, FileType::Regular | FileType::Directory) {
        lowers::layer_xattrs(pinned.xattrs()?)
    } else {
        Vec::new()
    };
    Ok(Some(Entry {
        path: entry_path(p)?,
        inode: (!is_dir && meta.nlink > 1).then_some((meta_dev(meta), meta.ino)),
        kind,
        mode: meta.mode,
        uid: u64::from(meta.uid),
        gid: u64::from(meta.gid),
        mtime: meta.mtime.secs,
        xattrs: xattrs.into_iter().map(|(n, v)| (n.into_vec(), v)).collect(),
    }))
}

/// The walk's `(dev, ino)` key, in the form of `crate::export::dev`.
fn meta_dev(meta: &EntryMeta) -> u64 {
    (u64::from(meta.dev.0) << 32) | u64::from(meta.dev.1)
}

/// Clears the hardlink key of every entry whose inode group does not agree
/// on type, size, mode, owner and mtime: names of one inode always agree,
/// so a disagreement means the inode number was reused during the walk,
/// and linking them would give one name the other's content.
fn split_reused_inodes(entries: &mut [Entry]) {
    let mut first: HashMap<(u64, u64), usize> = HashMap::new();
    let mut bad = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        let Some(key) = e.inode else { continue };
        match first.get(&key).and_then(|&j| entries.get(j)) {
            Some(f) => {
                if (&f.kind, f.mode, f.uid, f.gid, f.mtime)
                    != (&e.kind, e.mode, e.uid, e.gid, e.mtime)
                {
                    bad.insert(key);
                }
            }
            None => {
                first.insert(key, i);
            }
        }
    }
    for e in entries.iter_mut() {
        if e.inode.is_some_and(|k| bad.contains(&k)) {
            e.inode = None;
        }
    }
}

/// Pins `p` ([`RootPath::pin`]) and fails unless it is still the entry the
/// walk saw: the same type, device, inode, size, `mtime` and `ctime`.
fn pin_same(root: &RootPath, p: &Path, meta: &EntryMeta) -> Result<Pinned> {
    let pinned = root.pin(p)?;
    if same_stat(pinned.stat(), meta) {
        Ok(pinned)
    } else {
        Err(changed(p))
    }
}

/// The inode number alone is not enough: a file deleted and created again
/// may get the freed number back at once (ext4 does), so the size and the
/// times are compared too. Even they can match when both happen within one
/// timestamp tick; [`Verified`] checks the content for that.
fn same_stat(st: &Stat, meta: &EntryMeta) -> bool {
    st.file_type() == meta.kind
        && (st.dev_major, st.dev_minor) == meta.dev
        && st.ino == meta.ino
        && (st.file_type() != FileType::Regular || st.size == meta.size)
        && st.mtime == meta.mtime
        && st.ctime == meta.ctime
}

fn changed(p: &Path) -> Error {
    Error::Export {
        path: p.to_owned(),
        reason: "it changed between the walk and the export",
    }
}

/// File contents and parent directories for the writer, from the live
/// root.
struct Source<'a> {
    root: &'a RootPath,
    /// The walk's record of each regular file of the diff.
    files: HashMap<&'a Path, EntryMeta>,
    bytes_read: u64,
}

impl EntrySource for Source<'_> {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let p = Path::new(OsStr::from_bytes(entry.path.as_bytes()));
        let meta = *self
            .files
            .get(p)
            .ok_or_else(|| io::Error::other(format!("{} is not in the diff", p.display())))?;
        // Pinned with `O_PATH` on the physical path, checked to be the
        // walk's regular file, then reopened: a FIFO or device swapped in,
        // or a parent turned into a symlink or a mount, is never opened.
        let fd = pin_same(self.root, p, &meta)
            .and_then(|pinned| Ok(pinned.open_regular()?))
            .map_err(io::Error::other)?;
        if let EntryKind::File { size } = entry.kind {
            self.bytes_read += size;
        }
        let file = File::from(fd);
        Ok(match meta.content {
            Some(content) => Box::new(Verified {
                file,
                hash: Xxh3::new(),
                content,
                path: p.to_owned(),
            }),
            None => Box::new(file),
        })
    }

    fn parent(&mut self, path: &EntryPath) -> io::Result<Option<Entry>> {
        let p = Path::new(OsStr::from_bytes(path.as_bytes()));
        let pinned = self.root.pin(p).map_err(io::Error::other)?;
        let st = pinned.stat();
        if st.file_type() != FileType::Directory {
            return Ok(None);
        }
        let xattrs = pinned.xattrs().map_err(io::Error::other)?;
        Ok(Some(Entry {
            path: path.clone(),
            kind: EntryKind::Dir,
            mode: st.mode,
            uid: u64::from(st.uid),
            gid: u64::from(st.gid),
            mtime: st.mtime.secs,
            xattrs: lowers::layer_xattrs(xattrs)
                .into_iter()
                .map(|(n, v)| (n.into_vec(), v))
                .collect(),
            inode: None,
        }))
    }
}

/// A regular file's content, checked at its end against the hash the walk
/// read: a layer never carries other bytes than the walk compared, even if
/// a file with the same inode, size and times was swapped in between.
struct Verified {
    file: File,
    hash: Xxh3,
    /// The walk's xxh3-128 of the content.
    content: u128,
    path: PathBuf,
}

impl Read for Verified {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if n == 0 {
            if self.hash.digest128() != self.content {
                return Err(io::Error::other(changed(&self.path)));
            }
        } else {
            self.hash.update(buf.get(..n).unwrap_or_default());
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
