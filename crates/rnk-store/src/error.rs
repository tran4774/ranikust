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

    /// Another build holds the state directory, and one of the two is an
    /// anonymous store (kaniko mode on `/`): two containers sharing one
    /// volume would see and remove each other's names (ADR-0056), and a
    /// `Direct` `RUN` would reach a persistent store's entries and build
    /// directories (ADR-0077). Persistent stores share a state directory.
    #[error(
        "another build is using the state directory {}: a kaniko-mode build on / shares its \
         state directory with no other build; give each container its own volume or \
         --state-dir",
        path.display()
    )]
    Busy {
        /// The state directory.
        path: PathBuf,
    },

    /// An anonymous store (kaniko mode on `/`) was opened on a state
    /// directory holding more than its own marker and leftovers: a
    /// persistent store, or files ranikust did not leave there. A `Direct`
    /// `RUN` could change them for the builds that trust them (ADR-0077).
    #[error(
        "the state directory {} is not empty: it holds a persistent store or files ranikust \
         did not leave there; kaniko mode on / builds with a temporary store only, since a RUN \
         there could change what later builds reuse (ADR-0077); pass an empty or new \
         --state-dir, and keep persistent stores for overlay mode and per-stage builds",
        path.display()
    )]
    StateNotEmpty {
        /// The state directory.
        path: PathBuf,
    },

    /// A persistent store was opened on a state directory holding the
    /// marker of a kaniko-mode build on `/` (`.ranikust-building`): that
    /// build ended badly or is unknown, and its `RUN` may have written what
    /// a persistent store would reuse unchecked (ADR-0077).
    #[error(
        "the state directory {} was used by a kaniko-mode build on / that did not end \
         cleanly (it holds .ranikust-building): a RUN there may have written what a persistent \
         store would reuse, so it is not opened as one (ADR-0077); pass another --state-dir, \
         or delete this one first",
        path.display()
    )]
    UsedOnSlash {
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

    /// An anonymous store (kaniko mode on `/`) refuses what the caller
    /// asked: a path in the state directory a `RUN` could reach (ADR-0051),
    /// or a name while the fence between `RUN`s and names forbids one
    /// (ADR-0056, ADR-0066).
    #[error("kaniko mode on / refuses {what}: {why}")]
    Anonymous {
        /// What was asked, for example `RUN --mount=type=cache`.
        what: &'static str,
        /// Why, with the decision behind it.
        why: &'static str,
    },

    /// The kernel's random source failed, so no random name could be drawn
    /// for an anonymous file.
    #[error("drawing a random name with getrandom: {0}")]
    Random(#[source] io::Error),

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

    /// The state directory's filesystem keeps a name for a file that was
    /// unlinked while open (NFS's `.nfs*`, FUSE's `.fuse_hidden*`), so a
    /// file made there would stay reachable by a `RUN` in kaniko mode on `/`
    /// (ADR-0051). Never worked around with a name.
    #[error(
        "cannot keep state in {}: {why}, and a RUN in kaniko mode on / could open that name; \
         put the state directory on a local filesystem (ext4, xfs, btrfs or tmpfs)",
        path.display()
    )]
    NameOutlivesUnlink {
        /// The state directory.
        path: PathBuf,
        /// What keeps the name.
        why: String,
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
