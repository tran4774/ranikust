//! Errors of the store.

use std::io;
use std::path::{Path, PathBuf};

/// A store result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The state directory is not empty and carries no `.ranikust-state`
    /// marker file (or the marker is not a regular file), or, for
    /// [`crate::Store::prune_all`], holds a name the store does not make or
    /// a [`crate::PRUNING_MARKER`] that is not a regular file: it may be a
    /// directory the user cares about, which clean-up would delete from.
    #[error("{} {why}", path.display())]
    NotAStateDir {
        /// The state directory.
        path: PathBuf,
        /// What it holds or lacks, with any name quoted, and what to do.
        why: String,
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
    /// Also a build's store opened while [`crate::Store::prune_all`] empties
    /// the directory.
    #[error(
        "another build is using the state directory {}: a kaniko-mode build on / shares its \
         state directory with no other build, and ranikust prune --all empties it alone; wait \
         for it to end, or give each container its own volume or --state-dir",
        path.display()
    )]
    Busy {
        /// The state directory.
        path: PathBuf,
    },

    /// [`crate::Store::prune_all`] found a build holding the state
    /// directory: it removes nothing a running build uses (ADR-0105).
    #[error(
        "a build is using the state directory {}: ranikust prune --all empties it only while no \
         build runs there; run it again once they end",
        path.display()
    )]
    InUse {
        /// The state directory.
        path: PathBuf,
    },

    /// [`crate::Store::prune_all`] left more than [`crate::PRUNING_MARKER`]
    /// in the state directory: an entry on another mount, or one this
    /// process may not remove; that marker stays, so no build opens the
    /// directory and the next `prune --all` accepts it.
    #[error(
        "ranikust prune --all could not empty the state directory {}: it still holds {left}; \
         no build uses it until ranikust prune --all empties it",
        path.display()
    )]
    NotPruned {
        /// The state directory.
        path: PathBuf,
        /// What is left, with the names quoted.
        left: String,
    },

    /// The state directory holds [`crate::PRUNING_MARKER`]: a `ranikust
    /// prune --all` was cut short or failed there, and may have emptied
    /// entries part-way under their published names, which a build would
    /// reuse as they are. It names no way to finish, which depends on who
    /// runs the build: the caller's to give.
    #[error(
        "ranikust prune --all did not finish emptying the state directory {}: what it holds may \
         be partly removed, so no build uses it",
        path.display()
    )]
    PruneCutShort {
        /// The state directory.
        path: PathBuf,
    },

    /// An anonymous store (kaniko mode on `/`) was opened on a state
    /// directory holding more than its own marker and leftovers: a
    /// persistent store, or files ranikust did not leave there. A `Direct`
    /// `RUN` could change them for the builds that trust them (ADR-0077).
    #[error(
        "the state directory {} is not empty: it holds {found}; kaniko mode on / builds with a \
         temporary store only, since a RUN there could change what later builds reuse \
         (ADR-0077); pass an empty or new --state-dir, and keep persistent stores for overlay \
         mode and per-stage builds",
        path.display()
    )]
    StateNotEmpty {
        /// The state directory.
        path: PathBuf,
        /// What it holds, in words, with the names quoted.
        found: String,
    },

    /// An anonymous store could not empty its state directory at its end
    /// ([`crate::Store::close`]): an entry on another mount, or one that
    /// could not be removed. The directory keeps `.ranikust-building`, so no
    /// persistent store opens there (ADR-0077).
    #[error(
        "the state directory {} still holds {left} after a kaniko-mode build on /, which \
         ranikust could not remove; a RUN may have written it, so the directory keeps \
         .ranikust-building and no persistent store opens there: do not give this directory or \
         its volume to another build (ADR-0077)",
        path.display()
    )]
    NotEmptied {
        /// The state directory.
        path: PathBuf,
        /// What is left, with the names quoted.
        left: String,
    },

    /// An anonymous store could not write `.ranikust-building` at its end
    /// ([`crate::Store::close`]), not even once the state directory was
    /// emptied: a `RUN` may have written there what a persistent store
    /// would reuse, and nothing marks the directory (ADR-0077).
    #[error(
        "the state directory {} got no .ranikust-building marker after a kaniko-mode build on \
         / ({why}); {}: delete this directory, or its volume, before any other build uses it \
         (ADR-0077)",
        path.display(),
        unmarked_state(*emptied, *held)
    )]
    Unmarked {
        /// The state directory.
        path: PathBuf,
        /// Whether everything a `RUN` left there, but what holds the
        /// marker's name, was removed.
        emptied: bool,
        /// Whether something not ranikust's marker holds its name (a
        /// directory a `RUN` made that could not be removed).
        held: bool,
        /// Why the marker could not be written.
        why: String,
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

impl Error {
    /// Whether this is [`crate::Store::check_prune`]'s refusal to open the
    /// state directory `state` itself, as given, for a symbolic link on
    /// its path, as against a refusal of what it found there.
    #[must_use]
    pub fn is_symlink_on(&self, state: &Path) -> bool {
        matches!(
            self,
            Self::Rootfs(rnk_rootfs::Error::Io { path, source, .. })
                if path == state
                    && source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
        )
    }
}

/// What [`Error::Unmarked`] says of the state directory.
fn unmarked_state(emptied: bool, held: bool) -> &'static str {
    match (emptied, held) {
        (true, false) => "it was emptied, but nothing marks it",
        (true, true) => {
            "it was emptied except the name .ranikust-building, which holds something a RUN \
             made, not ranikust's marker"
        }
        (false, false) => "it still holds what a RUN may have written there, and nothing marks it",
        (false, true) => {
            "it still holds what a RUN may have written there, and the name .ranikust-building \
             holds something a RUN made, not ranikust's marker"
        }
    }
}
