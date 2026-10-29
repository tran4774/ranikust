//! The layer writer: sorts entries, adds missing parents, writes the tar.

use std::collections::btree_map::Entry as MapEntry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::num::NonZeroUsize;

use crate::digest::{HashingWriter, Sha256Digest};
use crate::entry::{OPAQUE_MARKER, WHITEOUT_PREFIX};
use crate::gzip::{Gzip, GzipEncoder};
use crate::header::{self, BLOCK_SIZE, Header, TypeFlag};
use crate::parallel::Digested;
use crate::zstd_frames::{Zstd, ZstdEncoder};
use crate::{Entry, EntryKind, EntryPath, Error};

/// Size of the buffer file content is copied through.
const COPY_BUFFER: usize = 128 * 1024;

/// The compression of a layer blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compression {
    /// A plain tar stream.
    None,
    /// gzip, written as one member by the parallel compressor.
    Gzip,
    /// zstd, written as one independent frame per 1 MiB of tar by the
    /// parallel compressor.
    Zstd,
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "uncompressed",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        })
    }
}

/// How a layer is encoded. Every field but `threads` is part of the
/// layer's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeOptions {
    /// The blob's compression.
    pub compression: Compression,
    /// The compression level: 0 to 9 for gzip, 1 to 19 for zstd; ignored
    /// when uncompressed.
    pub level: i32,
    /// Compression threads; above 1, two more threads hash the tar and the
    /// blob. The output does not depend on it.
    pub threads: NonZeroUsize,
    /// Under `--reproducible`, the mtime of every header, in seconds since
    /// the Unix epoch; `None` keeps each entry's own mtime.
    pub mtime: Option<i64>,
}

/// What [`LayerWriter::finish`] computed while writing a layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerDigests {
    /// sha256 of the uncompressed tar: the image config's `diff_id`.
    pub diff_id: Sha256Digest,
    /// sha256 of the blob as written: the descriptor's `digest`.
    pub digest: Sha256Digest,
    /// Size of the blob as written, in bytes.
    pub size: u64,
    /// The blob's compression; the media type follows from it and the
    /// image format, which `rnk-oci` owns.
    pub compression: Compression,
}

/// Where the writer gets what the entries alone do not carry.
pub trait EntrySource {
    /// Opens the content of a regular file entry, positioned at its start.
    /// The writer reads exactly the entry's size, then checks for the end.
    /// It is not called for empty files or for files written as hardlinks.
    ///
    /// # Errors
    ///
    /// Any I/O error; the writer reports it as [`Error::Source`].
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>>;

    /// The metadata of the directory `path`, an ancestor of a fed entry that
    /// was not fed itself; `None` if it is unknown. It must be a
    /// [`EntryKind::Dir`] entry at `path`.
    ///
    /// [open: Q-013] Interim: the exporter answers lookups, so it may feed
    /// only changed entries (BuildKit's `sameDirent`) and still have every
    /// ancestor written with its real metadata.
    ///
    /// # Errors
    ///
    /// Any I/O error; the writer reports it as [`Error::Source`].
    fn parent(&mut self, path: &EntryPath) -> io::Result<Option<Entry>>;
}

/// Collects the entries of one layer, then writes them as a deterministic
/// tar stream.
///
/// Entries may be added in any order; [`finish`](Self::finish) sorts them,
/// asks the [`EntrySource`] for missing parent directories, turns entries
/// sharing an inode into hardlinks and streams the tar. The bytes depend
/// only on the entries, their content and the [`EncodeOptions`] other than
/// `threads`.
///
/// # Layer order
///
/// Depth first, components compared byte-wise (`a`, `a/b`, `a-b`); within
/// a directory the opaque marker comes first, then whiteouts, then the other
/// entries, so whiteouts precede their siblings as the OCI layer
/// specification recommends (Q-053).
#[derive(Debug)]
pub struct LayerWriter {
    options: EncodeOptions,
    entries: BTreeMap<EntryPath, Entry>,
}

impl LayerWriter {
    /// A writer with no entries yet.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidLevel`] for a level the compression does not have.
    pub fn new(options: EncodeOptions) -> Result<Self, Error> {
        match options.compression {
            Compression::None => {}
            Compression::Gzip => {
                gzip_level(options.level)?;
            }
            Compression::Zstd => {
                zstd_level(options.level)?;
            }
        }
        Ok(Self {
            options,
            entries: BTreeMap::new(),
        })
    }

    /// Adds one entry.
    ///
    /// # Errors
    ///
    /// [`Error::DuplicatePath`] if the path was added before;
    /// [`Error::InvalidEntry`] if the entry cannot be written (a name
    /// reserved for whiteouts, an invalid xattr name or symlink target).
    pub fn add(&mut self, entry: Entry) -> Result<(), Error> {
        entry.validate()?;
        match self.entries.entry(entry.path.clone()) {
            MapEntry::Occupied(_) => Err(Error::DuplicatePath(entry.path)),
            MapEntry::Vacant(slot) => {
                slot.insert(entry);
                Ok(())
            }
        }
    }

    /// The number of entries added so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry was added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Writes the layer to `out`, which is flushed, and returns its digests,
    /// computed while streaming.
    ///
    /// # Errors
    ///
    /// [`Error::MissingParent`] or [`Error::ParentNotDirectory`] for an
    /// ancestor that cannot be written as a directory, [`Error::Source`] when
    /// `source` fails, [`Error::ContentShorter`] or [`Error::ContentLonger`]
    /// when a file's size changed, [`Error::Io`] when writing fails.
    pub fn finish<W: Write>(
        mut self,
        source: &mut dyn EntrySource,
        out: W,
    ) -> Result<LayerDigests, Error> {
        let compression = self.options.compression;
        let span = tracing::debug_span!(
            "rnk_layer.encode",
            entries = tracing::field::Empty,
            parents_added = tracing::field::Empty,
            bytes_in = tracing::field::Empty,
            bytes_out = tracing::field::Empty,
            max_threads = self.options.threads.get(),
        );
        let _entered = span.enter();
        let fed = self.entries.len();
        self.add_parents(source)?;
        span.record("entries", self.entries.len());
        span.record("parents_added", self.entries.len() - fed);
        let records = records(&self.entries);
        let threads = self.options.threads;
        let sink = match compression {
            Compression::None => Sink::Plain(HashingWriter::new(BufWriter::with_capacity(
                COPY_BUFFER,
                out,
            ))),
            Compression::Gzip => {
                let level = gzip_level(self.options.level)?;
                Sink::Gzip(Box::new(GzipEncoder::new(out, Gzip { level }, threads)?))
            }
            Compression::Zstd => {
                let level = zstd_level(self.options.level)?;
                Sink::Zstd(Box::new(ZstdEncoder::new(out, Zstd { level }, threads)?))
            }
        };
        let mut tar = Tar {
            out: sink,
            header: Vec::with_capacity(4 * BLOCK_SIZE),
            buffer: vec![0; COPY_BUFFER],
            mtime: self.options.mtime,
        };
        for record in &records {
            tar.write(record, source)?;
        }
        tar.out.write_all(&[0; 2 * BLOCK_SIZE])?;
        let ((diff_id, tar_size), (digest, size)) = tar.out.finish()?;
        span.record("bytes_in", tar_size);
        span.record("bytes_out", size);
        Ok(LayerDigests {
            diff_id,
            digest,
            size,
            compression,
        })
    }

    /// Looks up every ancestor that was not fed, outermost first.
    fn add_parents(&mut self, source: &mut dyn EntrySource) -> Result<(), Error> {
        let mut missing = BTreeSet::new();
        for path in self.entries.keys() {
            for ancestor in path.ancestors() {
                match self.entries.get(&ancestor) {
                    Some(entry) if entry.kind.is_dir() => {}
                    Some(_) => {
                        return Err(Error::ParentNotDirectory {
                            path: path.clone(),
                            parent: ancestor,
                        });
                    }
                    None => {
                        missing.insert(ancestor);
                    }
                }
            }
        }
        for path in missing {
            let found = source.parent(&path).map_err(|source| Error::Source {
                path: path.clone(),
                source,
            })?;
            let Some(entry) = found else {
                return Err(Error::MissingParent(path));
            };
            if entry.path != path || entry.kind != EntryKind::Dir {
                return Err(Error::InvalidEntry {
                    path,
                    reason: "the source's parent lookup did not give a directory at this path",
                });
            }
            entry.validate()?;
            self.entries.insert(path, entry);
        }
        Ok(())
    }
}

/// One tar entry to write.
#[derive(Debug)]
enum Record<'a> {
    /// The entry itself; a hardlink to `link` if set.
    Entry {
        entry: &'a Entry,
        link: Option<&'a EntryPath>,
    },
    /// `<path>/.wh..wh..opq` of an opaque directory.
    Opaque(&'a Entry),
    /// `<parent>/.wh.<name>` of a deleted entry.
    Whiteout(&'a Entry),
}

/// Sort ranks within a directory: the opaque marker, whiteouts, the rest.
const RANK_OPAQUE: u8 = 0;
const RANK_WHITEOUT: u8 = 1;
const RANK_ENTRY: u8 = 2;

impl<'a> Record<'a> {
    fn sort_key(&self) -> Vec<(u8, &'a [u8])> {
        let (entry, last) = match self {
            Self::Entry { entry, .. } => (entry, None),
            Self::Opaque(entry) => (entry, Some((RANK_OPAQUE, &b""[..]))),
            Self::Whiteout(entry) => (entry, None),
        };
        let mut key: Vec<(u8, &'a [u8])> =
            entry.path.components().map(|c| (RANK_ENTRY, c)).collect();
        if let Self::Whiteout(_) = self
            && let Some(name) = key.last_mut()
        {
            name.0 = RANK_WHITEOUT;
        }
        key.extend(last);
        key
    }
}

/// The records of `entries` in layer order, with hardlinks resolved.
fn records(entries: &BTreeMap<EntryPath, Entry>) -> Vec<Record<'_>> {
    let mut records = Vec::with_capacity(entries.len());
    for entry in entries.values() {
        match entry.kind {
            EntryKind::Whiteout => records.push(Record::Whiteout(entry)),
            EntryKind::OpaqueDir => {
                records.push(Record::Entry { entry, link: None });
                records.push(Record::Opaque(entry));
            }
            EntryKind::File { .. }
            | EntryKind::Dir
            | EntryKind::Symlink(_)
            | EntryKind::Char(..)
            | EntryKind::Block(..)
            | EntryKind::Fifo => records.push(Record::Entry { entry, link: None }),
        }
    }
    records.sort_by_cached_key(Record::sort_key);

    let mut first_path: HashMap<(u64, u64), &EntryPath> = HashMap::new();
    for record in &mut records {
        if let Record::Entry { entry, link } = record
            && !entry.kind.is_dir()
            && let Some(inode) = entry.inode
        {
            match first_path.get(&inode) {
                Some(&first) => *link = Some(first),
                None => {
                    first_path.insert(inode, &entry.path);
                }
            }
        }
    }
    records
}

/// Checks a gzip level.
fn gzip_level(level: i32) -> Result<u32, Error> {
    u32::try_from(level)
        .ok()
        .filter(|&l| l <= 9)
        .ok_or(Error::InvalidLevel {
            compression: Compression::Gzip,
            level,
        })
}

/// Checks a zstd level. Levels 20 to 22 ("ultra") need large windows and
/// negative levels trade most of the ratio away; neither is offered.
fn zstd_level(level: i32) -> Result<i32, Error> {
    if (1..=19).contains(&level) {
        Ok(level)
    } else {
        Err(Error::InvalidLevel {
            compression: Compression::Zstd,
            level,
        })
    }
}

/// Where the tar stream goes: hashed and written out, or through a
/// compressor that hashes its input and its output.
enum Sink<W: Write> {
    Plain(HashingWriter<BufWriter<W>>),
    Gzip(Box<GzipEncoder<W>>),
    Zstd(Box<ZstdEncoder<W>>),
}

impl<W: Write> Sink<W> {
    /// Ends the stream and flushes the output. Returns the sha256 and size
    /// of the tar, then of the blob.
    fn finish(self) -> io::Result<(Digested, Digested)> {
        let (mut out, tar, blob) = match self {
            Self::Plain(out) => {
                let tar = out.hasher.finish();
                let out = out
                    .inner
                    .into_inner()
                    .map_err(io::IntoInnerError::into_error)?;
                (out, tar, tar)
            }
            Self::Gzip(out) => out.finish()?,
            Self::Zstd(out) => out.finish()?,
        };
        out.flush()?;
        Ok((tar, blob))
    }
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(out) => out.write(buf),
            Self::Gzip(out) => out.write(buf),
            Self::Zstd(out) => out.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(out) => out.write_all(buf),
            Self::Gzip(out) => out.write_all(buf),
            Self::Zstd(out) => out.write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(out) => out.flush(),
            Self::Gzip(out) => out.flush(),
            Self::Zstd(out) => out.flush(),
        }
    }
}

/// The tar stream being written.
struct Tar<W: Write> {
    out: Sink<W>,
    header: Vec<u8>,
    buffer: Vec<u8>,
    mtime: Option<i64>,
}

impl<W: Write> Tar<W> {
    fn write(&mut self, record: &Record<'_>, source: &mut dyn EntrySource) -> Result<(), Error> {
        let (entry, name, h) = match *record {
            Record::Entry { entry, link } => {
                let mut name = entry.path.as_bytes().to_vec();
                let h = entry_header(entry, link, self.mtime);
                if entry.kind.is_dir() {
                    name.push(b'/');
                }
                (entry, name, h)
            }
            Record::Opaque(entry) => {
                let mut name = entry.path.as_bytes().to_vec();
                name.push(b'/');
                name.extend_from_slice(OPAQUE_MARKER);
                (entry, name, marker_header(entry, self.mtime))
            }
            Record::Whiteout(entry) => {
                let mut name = entry.path.parent().map_or_else(Vec::new, |parent| {
                    let mut name = parent.as_bytes().to_vec();
                    name.push(b'/');
                    name
                });
                name.extend_from_slice(WHITEOUT_PREFIX);
                name.extend_from_slice(entry.path.file_name());
                (entry, name, marker_header(entry, self.mtime))
            }
        };
        let h = Header { name: &name, ..h };
        self.header.clear();
        header::encode(&h, &mut self.header).map_err(|e| Error::InvalidEntry {
            path: entry.path.clone(),
            reason: e.reason(),
        })?;
        self.out.write_all(&self.header)?;
        if h.typeflag == TypeFlag::Regular && h.size > 0 {
            self.copy_content(entry, h.size, source)?;
        }
        Ok(())
    }

    /// Copies exactly `size` bytes of `entry` from the source, then pads.
    fn copy_content(
        &mut self,
        entry: &Entry,
        size: u64,
        source: &mut dyn EntrySource,
    ) -> Result<(), Error> {
        let source_error = |source| Error::Source {
            path: entry.path.clone(),
            source,
        };
        let mut reader = source.open(entry).map_err(source_error)?;
        let mut left = size;
        while left > 0 {
            let want =
                usize::try_from(left).map_or(self.buffer.len(), |left| left.min(self.buffer.len()));
            let n = match reader.read(&mut self.buffer[..want]) {
                Ok(0) => {
                    return Err(Error::ContentShorter {
                        path: entry.path.clone(),
                        expected: size,
                        actual: size - left,
                    });
                }
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(source_error(e)),
            };
            self.out.write_all(&self.buffer[..n])?;
            left -= u64::try_from(n).map_err(io::Error::other)?;
        }
        loop {
            match reader.read(&mut self.buffer[..1]) {
                Ok(0) => break,
                Ok(_) => {
                    return Err(Error::ContentLonger {
                        path: entry.path.clone(),
                        expected: size,
                    });
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(source_error(e)),
            }
        }
        self.out
            .write_all(&[0; BLOCK_SIZE][..header::padding(size)])?;
        Ok(())
    }
}

/// The header of an entry, without its name.
fn entry_header<'a>(
    entry: &'a Entry,
    link: Option<&'a EntryPath>,
    mtime: Option<i64>,
) -> Header<'a> {
    let (typeflag, size, linkname, device): (_, _, &[u8], _) = match &entry.kind {
        EntryKind::File { size } => (TypeFlag::Regular, *size, b"", (0, 0)),
        EntryKind::Dir | EntryKind::OpaqueDir => (TypeFlag::Dir, 0, b"", (0, 0)),
        EntryKind::Symlink(target) => (TypeFlag::Symlink, 0, target, (0, 0)),
        EntryKind::Char(major, minor) => (TypeFlag::Char, 0, b"", (*major, *minor)),
        EntryKind::Block(major, minor) => (TypeFlag::Block, 0, b"", (*major, *minor)),
        EntryKind::Fifo => (TypeFlag::Fifo, 0, b"", (0, 0)),
        // Written by `marker_header`; never an entry record.
        EntryKind::Whiteout => (TypeFlag::Regular, 0, b"", (0, 0)),
    };
    let (typeflag, size, linkname, device) = match link {
        Some(first) => (TypeFlag::HardLink, 0, first.as_bytes(), (0, 0)),
        None => (typeflag, size, linkname, device),
    };
    Header {
        name: b"",
        typeflag,
        mode: entry.mode,
        uid: entry.uid,
        gid: entry.gid,
        size,
        mtime: mtime.unwrap_or(entry.mtime),
        linkname,
        device,
        xattrs: (!entry.xattrs.is_empty()).then_some(&entry.xattrs),
    }
}

/// The header of a whiteout or opaque marker: an empty file, mode 0, 0:0,
/// as kaniko writes it (`kaniko@44aa7ddc0 pkg/util/tar_util.go:162-176`).
fn marker_header(entry: &Entry, mtime: Option<i64>) -> Header<'static> {
    Header {
        name: b"",
        typeflag: TypeFlag::Regular,
        mode: 0,
        uid: 0,
        gid: 0,
        size: 0,
        mtime: mtime.unwrap_or(entry.mtime),
        linkname: b"",
        device: (0, 0),
        xattrs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> EntryPath {
        EntryPath::new(s).unwrap()
    }

    fn entry(p: &str, kind: EntryKind) -> Entry {
        Entry {
            path: path(p),
            kind,
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: 1,
            xattrs: BTreeMap::new(),
            inode: None,
        }
    }

    fn names(entries: Vec<Entry>) -> Vec<String> {
        let map: BTreeMap<EntryPath, Entry> =
            entries.into_iter().map(|e| (e.path.clone(), e)).collect();
        records(&map)
            .iter()
            .map(|r| match r {
                Record::Entry { entry, link } => {
                    let link = link.map_or(String::new(), |l| format!(" -> {l}"));
                    format!("{}{link}", entry.path)
                }
                Record::Opaque(e) => {
                    format!("{}/{}", e.path, String::from_utf8_lossy(OPAQUE_MARKER))
                }
                Record::Whiteout(e) => format!("whiteout {}", e.path),
            })
            .collect()
    }

    #[test]
    fn markers_come_first_in_their_directory() {
        let got = names(vec![
            entry("d/-dash", EntryKind::File { size: 0 }),
            entry("d/b", EntryKind::Whiteout),
            entry("d", EntryKind::OpaqueDir),
            entry("d/a", EntryKind::Dir),
            entry("d/a/x", EntryKind::Whiteout),
            entry("d-e", EntryKind::Fifo),
            entry("c", EntryKind::Whiteout),
        ]);
        assert_eq!(
            got,
            [
                "whiteout c",
                "d",
                "d/.wh..wh..opq",
                "whiteout d/b",
                "d/-dash",
                "d/a",
                "whiteout d/a/x",
                "d-e",
            ]
        );
    }

    #[test]
    fn hardlinks_point_to_the_first_path_in_layer_order() {
        let mut a = entry("z/a", EntryKind::File { size: 3 });
        let mut b = entry("b", EntryKind::File { size: 3 });
        let mut c = entry("y", EntryKind::Symlink(b"t".to_vec()));
        let mut d = entry("dir", EntryKind::Dir);
        a.inode = Some((1, 7));
        b.inode = Some((1, 7));
        c.inode = Some((2, 7));
        d.inode = Some((1, 7));
        assert_eq!(names(vec![a, b, c, d]), ["b", "dir", "y", "z/a -> b"]);
    }
}
