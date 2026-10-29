//! The crate's error type.

use std::io;

use crate::{Compression, EntryPath};

/// Everything that can go wrong while encoding or decoding a layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A path is not a normalized relative path.
    #[error("invalid layer path {path:?}: {reason}")]
    InvalidPath {
        /// The rejected path, lossily decoded.
        path: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// An entry cannot be written to a layer.
    #[error("cannot write layer entry {path}: {reason}")]
    InvalidEntry {
        /// The entry's path.
        path: EntryPath,
        /// Why it cannot be written.
        reason: &'static str,
    },
    /// Two entries have the same path (a whiteout and an entry included).
    #[error("layer entry {0} was added twice")]
    DuplicatePath(EntryPath),
    /// An entry's ancestor was neither fed nor known to the source.
    #[error("no metadata for the parent directory {0}")]
    MissingParent(EntryPath),
    /// An entry's ancestor is in the layer but is not a directory.
    #[error("layer entry {path} has a parent {parent} that is not a directory")]
    ParentNotDirectory {
        /// The entry below the parent.
        path: EntryPath,
        /// The ancestor that is not a directory.
        parent: EntryPath,
    },
    /// The compression level is out of range for the algorithm.
    #[error("compression level {level} is out of range for {compression}")]
    InvalidLevel {
        /// The algorithm.
        compression: Compression,
        /// The rejected level.
        level: i32,
    },
    /// The compression is recognised but not implemented yet.
    // zstd needs a C toolchain for the static musl build; it arrives with
    // cargo-zigbuild (Q-052, ADR-0020).
    #[error("{0} layers are not supported yet")]
    Unsupported(Compression),
    /// A file gave fewer bytes than its entry announced.
    #[error("{path} gave {actual} bytes, but its entry announced {expected}")]
    ContentShorter {
        /// The file's path.
        path: EntryPath,
        /// The size in the entry.
        expected: u64,
        /// What the source gave before its end.
        actual: u64,
    },
    /// A file gave more bytes than its entry announced.
    #[error("{path} gave more than the {expected} bytes its entry announced")]
    ContentLonger {
        /// The file's path.
        path: EntryPath,
        /// The size in the entry.
        expected: u64,
    },
    /// The entry source failed to open or read an entry, or to look up a
    /// parent directory.
    #[error("reading layer entry {path} from its source failed")]
    Source {
        /// The entry being read or looked up.
        path: EntryPath,
        /// The source's error.
        #[source]
        source: io::Error,
    },
    /// Writing the layer or reading a blob failed.
    #[error("layer I/O failed")]
    Io(#[from] io::Error),
}
