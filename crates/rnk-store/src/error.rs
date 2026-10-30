//! Errors of the store.

use std::io;
use std::path::PathBuf;

/// A store result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The state directory is not empty and carries no `.ranikust-root`
    /// marker file (or the marker is not a regular file): it may be a
    /// directory the user cares about, which clean-up would delete from.
    #[error(
        "{} is not empty and has no regular file .ranikust-root; \
         refusing to use it as the state directory (pass an empty or new --state-dir)",
        path.display()
    )]
    NotAStateDir {
        /// The state directory.
        path: PathBuf,
    },

    /// The filesystem of the state directory does not support `flock`,
    /// which the locks between builds rely on.
    #[error(
        "the state directory {} does not support flock ({source}); \
         pass a --state-dir on a local filesystem",
        path.display()
    )]
    NoFlock {
        /// The state directory.
        path: PathBuf,
        /// What the kernel answered.
        #[source]
        source: io::Error,
    },

    /// A host path operation on the state directory itself failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// The operation, for example `create`.
        op: &'static str,
        /// The host path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// Something else than the store's own entry sits under a name in the
    /// state directory: a symlink, the wrong type, a mount point, or a
    /// directory swapped while it was being deleted. It is neither followed
    /// nor replaced.
    #[error("state directory entry {}: {reason}", path.display())]
    BadEntry {
        /// The entry, relative to the state directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// An operation inside the state directory failed.
    #[error("state directory: {0}")]
    Rootfs(#[from] rnk_rootfs::Error),

    /// The caller's filler failed; nothing was published.
    #[error("filling {key}: {source}")]
    Fill {
        /// The entry, for example `blob sha256:…`.
        key: String,
        /// The filler's error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
