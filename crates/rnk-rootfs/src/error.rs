//! The crate's error type.

use std::io;
use std::path::PathBuf;

/// Errors of `rnk-rootfs`.
///
/// Paths in errors are paths inside the root (as the caller passed them, or as
/// a tar entry named them), never host paths of the root.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A kernel feature `RootPath` cannot work without is missing or blocked.
    ///
    /// There is no fallback: resolving paths by joining strings is exactly
    /// what this crate exists to prevent.
    #[error(
        "{feature} is unavailable ({source}); ranikust needs Linux 5.8 or later \
         and a seccomp profile that allows openat2 and statx"
    )]
    Unsupported {
        /// The syscall or interface that failed, for example `openat2`.
        feature: &'static str,
        /// What the kernel answered.
        #[source]
        source: io::Error,
    },

    /// A filesystem operation failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// The operation, for example `open` or `unlink`.
        op: &'static str,
        /// The path inside the root.
        path: PathBuf,
        /// The underlying error; `source.kind()` and `source.raw_os_error()`
        /// tell, for example, a missing file from a loop.
        #[source]
        source: io::Error,
    },

    /// The path cannot be used for this operation, for example a path that
    /// names the root itself where a directory entry is needed.
    #[error("invalid path {}: {reason}", path.display())]
    InvalidPath {
        /// The path inside the root.
        path: PathBuf,
        /// Why it is refused.
        reason: &'static str,
    },

    /// The tar stream itself is malformed or could not be read.
    #[error("reading the tar stream: {0}")]
    Tar(#[source] io::Error),

    /// A tar entry is refused, for example because its name or hardlink
    /// target leaves the root.
    #[error("tar entry {}: {reason}", path.display())]
    Entry {
        /// The entry name as found in the archive.
        path: PathBuf,
        /// Why it is refused.
        reason: String,
    },

    /// A directory entry changed while it was being deleted (a swap between
    /// `statx` and `open`); deletion stops instead of guessing.
    #[error("{} changed while it was being deleted", path.display())]
    Changed {
        /// The path inside the root.
        path: PathBuf,
    },

    /// A `USER` or `--chown` specification names no known user or group, or
    /// an id out of range.
    #[error("{0}")]
    User(String),
}

impl Error {
    /// The underlying I/O error, if this error wraps one.
    #[must_use]
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            Self::Unsupported { source, .. } | Self::Io { source, .. } | Self::Tar(source) => {
                Some(source)
            }
            Self::InvalidPath { .. }
            | Self::Entry { .. }
            | Self::Changed { .. }
            | Self::User(_) => None,
        }
    }

    /// The raw `errno` of the underlying I/O error, if any.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.io_error().and_then(io::Error::raw_os_error)
    }
}

/// The result type of `rnk-rootfs`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Builds an [`Error::Io`] from a rustix errno.
pub(crate) fn io_err(op: &'static str, path: &std::path::Path, e: rustix::io::Errno) -> Error {
    Error::Io {
        op,
        path: path.to_owned(),
        source: e.into(),
    }
}
