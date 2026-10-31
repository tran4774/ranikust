//! Errors of the store.

use std::io;
use std::path::PathBuf;

/// A store result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The state directory is not empty and carries no `.ranikust-state`
    /// marker file (or the marker is not a regular file): it may be a
    /// directory the user cares about, which clean-up would delete from.
    #[error(
        "{} is not empty and has no regular file .ranikust-state; \
         refusing to use it as the state directory (pass an empty or new --state-dir; \
         a state directory of an earlier ranikust, marked .ranikust-root, is not reused: \
         delete it first)",
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

    /// Another build holds the state directory of an anonymous store (two
    /// containers sharing one volume): it would see and remove this one's
    /// names (ADR-0056).
    #[error(
        "another build is using the state directory {}: one build at a time per state \
         directory; give each container its own volume or --state-dir",
        path.display()
    )]
    Busy {
        /// The state directory.
        path: PathBuf,
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

    /// The caller cancelled while waiting for a locked cache mount.
    #[error("cancelled while waiting for a locked cache mount")]
    Cancelled,

    /// What the caller asked for needs a path in the state directory, and
    /// the store keeps nothing there (kaniko mode on `/`, ADR-0051).
    #[error(
        "{0} is not available in kaniko mode on /: the store keeps nothing a RUN could \
         reach (ADR-0051)"
    )]
    Anonymous(&'static str),

    /// An anonymous file could not be made: neither `O_TMPFILE` in the state
    /// directory nor `memfd_create`.
    #[error("cannot make an anonymous file in {}: {source}", path.display())]
    AnonymousFile {
        /// The state directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
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
