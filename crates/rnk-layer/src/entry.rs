//! What the layer writer is fed: validated paths and entry metadata.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use crate::Error;

/// Prefix that marks a whiteout file in an OCI layer.
pub(crate) const WHITEOUT_PREFIX: &[u8] = b".wh.";
/// Name of the opaque whiteout file in an OCI layer.
pub(crate) const OPAQUE_MARKER: &[u8] = b".wh..wh..opq";

/// A path inside a layer, relative to the layer root.
///
/// Raw bytes (Linux file names need not be UTF-8), normalized: not empty, no
/// leading or trailing `/`, no empty, `.` or `..` component, no NUL byte. The
/// root itself is never a layer entry, so it has no `EntryPath`.
///
/// Paths order component by component, byte-wise within a component, so a
/// directory sorts right before its own descendants (`a`, `a/b`, `a-b`).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EntryPath(Box<[u8]>);

impl EntryPath {
    /// Validates `path` as a normalized relative path.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] if the path is empty, absolute, has a trailing
    /// `/`, an empty, `.` or `..` component, or a NUL byte.
    pub fn new(path: impl Into<Vec<u8>>) -> Result<Self, Error> {
        let path = path.into();
        let reason = if path.is_empty() {
            Some("empty path")
        } else if path.contains(&0) {
            Some("contains a NUL byte")
        } else if path.first() == Some(&b'/') {
            Some("absolute path")
        } else if path.last() == Some(&b'/') {
            Some("trailing slash")
        } else {
            path.split(|&b| b == b'/')
                .find_map(|component| match component {
                    b"" => Some("empty component"),
                    b"." | b".." => Some("`.` or `..` component"),
                    _ => None,
                })
        };
        match reason {
            Some(reason) => Err(Error::InvalidPath {
                path: String::from_utf8_lossy(&path).into_owned(),
                reason,
            }),
            None => Ok(Self(path.into_boxed_slice())),
        }
    }

    /// The raw bytes, components separated by `/`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The components, first to last.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.0.split(|&b| b == b'/')
    }

    /// The last component.
    #[must_use]
    pub fn file_name(&self) -> &[u8] {
        self.split_last().1
    }

    /// The parent directory, or `None` for a path directly below the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        self.split_last().0.map(|parent| Self(parent.into()))
    }

    /// Every proper ancestor, outermost first (`a/b/c` gives `a`, `a/b`).
    pub(crate) fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.0
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'/')
            .map(|(i, _)| Self(self.0[..i].into()))
    }

    fn split_last(&self) -> (Option<&[u8]>, &[u8]) {
        match self.0.iter().rposition(|&b| b == b'/') {
            Some(i) => (Some(&self.0[..i]), &self.0[i + 1..]),
            None => (None, &self.0),
        }
    }
}

impl Ord for EntryPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.components().cmp(other.components())
    }
}

impl PartialOrd for EntryPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for EntryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for EntryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&String::from_utf8_lossy(&self.0), f)
    }
}

/// One entry of a layer, as the exporter sees it on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the entry lives in the layer.
    pub path: EntryPath,
    /// What the entry is.
    pub kind: EntryKind,
    /// The raw `st_mode`. Only the permission, setuid, setgid and sticky bits
    /// (`0o7777`) are written; the file type comes from [`kind`](Self::kind).
    pub mode: u32,
    /// Numeric owner; user names are never written.
    pub uid: u64,
    /// Numeric group; group names are never written.
    pub gid: u64,
    /// Modification time in whole seconds since the Unix epoch. Replaced by
    /// [`EncodeOptions::mtime`](crate::EncodeOptions::mtime) when that is set.
    pub mtime: i64,
    /// Extended attributes, written as PAX `SCHILY.xattr.<name>` records.
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// `(st_dev, st_ino)`; entries sharing it are written as hardlinks to the
    /// first of them in layer order. `None` when the entry has one link.
    /// Ignored for directories, whiteouts and opaque directories.
    pub inode: Option<(u64, u64)>,
}

/// The kind of a layer [`Entry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file of `size` bytes, read from the
    /// [`EntrySource`](crate::EntrySource) while the layer is written.
    File {
        /// Content length in bytes; the source must give exactly this many.
        size: u64,
    },
    /// A directory.
    Dir,
    /// A symbolic link to the given target.
    Symlink(Vec<u8>),
    /// A character device `(major, minor)`.
    Char(u32, u32),
    /// A block device `(major, minor)`.
    Block(u32, u32),
    /// A named pipe.
    Fifo,
    /// The entry's path was deleted: written as the empty regular file
    /// `<parent>/.wh.<name>` with mode 0 and owner 0:0. Only the path and
    /// the mtime of the entry are used.
    Whiteout,
    /// A directory that hides everything below it in lower layers: written as
    /// the directory, then the empty file `<path>/.wh..wh..opq` with mode 0,
    /// owner 0:0 and the directory's mtime.
    OpaqueDir,
}

impl EntryKind {
    /// Whether the entry is a directory in the layer.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        match self {
            Self::Dir | Self::OpaqueDir => true,
            Self::File { .. }
            | Self::Symlink(_)
            | Self::Char(..)
            | Self::Block(..)
            | Self::Fifo
            | Self::Whiteout => false,
        }
    }
}

impl Entry {
    /// Checks what the path type cannot: names reserved for whiteouts,
    /// xattr names and symlink targets.
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let invalid = |reason| {
            Err(Error::InvalidEntry {
                path: self.path.clone(),
                reason,
            })
        };
        // [open: Q-053] A file really named `.wh.*` cannot be represented: an
        // extractor would take it for a whiteout. Rejected for now.
        if self.path.file_name().starts_with(WHITEOUT_PREFIX) {
            return invalid("the name starts with `.wh.`, reserved for whiteouts");
        }
        for name in self.xattrs.keys() {
            if name.is_empty() || name.contains(&0) || name.contains(&b'=') {
                return invalid("an xattr name is empty or contains NUL or `=`");
            }
        }
        if let EntryKind::Symlink(target) = &self.kind
            && (target.is_empty() || target.contains(&0))
        {
            return invalid("the symlink target is empty or contains NUL");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> EntryPath {
        EntryPath::new(s).unwrap()
    }

    #[test]
    fn rejects_non_normalized_paths() {
        for bad in ["", "/a", "a/", "a//b", "./a", "a/./b", "a/..", "..", "a\0b"] {
            assert!(EntryPath::new(bad).is_err(), "{bad:?} accepted");
        }
        for good in ["a", "a/b", ".a/..b/...", "a b/ü"] {
            assert!(EntryPath::new(good).is_ok(), "{good:?} rejected");
        }
    }

    #[test]
    fn orders_component_wise() {
        let mut paths = [p("a-b"), p("a/b"), p("a"), p("a/b/c"), p("a.b"), p("B")];
        paths.sort();
        let got: Vec<String> = paths.iter().map(ToString::to_string).collect();
        assert_eq!(got, ["B", "a", "a/b", "a/b/c", "a-b", "a.b"]);
    }

    #[test]
    fn splits_parent_and_name() {
        assert_eq!(p("a/b/c").parent(), Some(p("a/b")));
        assert_eq!(p("a").parent(), None);
        assert_eq!(p("a/b/c").file_name(), b"c");
        assert_eq!(p("a").file_name(), b"a");
        let ancestors: Vec<EntryPath> = p("a/b/c").ancestors().collect();
        assert_eq!(ancestors, [p("a"), p("a/b")]);
        assert_eq!(p("a").ancestors().count(), 0);
    }
}
