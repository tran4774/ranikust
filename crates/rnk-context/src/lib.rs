//! Build context sources.
//!
//! Provides the build context from a local directory, a tar archive, a git
//! repository or an HTTPS URL, behind an open trait so that new sources can be
//! added later. Locates the ignore file: `<Dockerfile>.dockerignore` takes
//! precedence over `.dockerignore` at the context root, as in kaniko and
//! BuildKit. Walks the context exactly as BuildKit's filtered walk does
//! (ADR-0027), reporting each entry `COPY` may read.
//!
//! Phase 1 has local directories only; tar, git and HTTPS contexts are
//! parsed and refused until Phase 5.
//!
//! ```no_run
//! # fn main() -> Result<(), rnk_context::Error> {
//! let ctx = rnk_context::open(&rnk_context::parse_spec("app")?)?;
//! let ignore = ctx.find_dockerignore(Some("app/Dockerfile".as_ref()))?;
//! let matcher = ignore.map(|f| ctx.matcher(&f)).transpose()?;
//! for entry in ctx.walk(matcher.as_ref()) {
//!     let entry = entry?;
//!     println!("{}", entry.path.display());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Boundaries
//!
//! - No cloud object-store sources (S3, GCS, Azure) for now.
//! - Does not interpret the Dockerfile or decide which files an instruction
//!   copies (`rnk-builder`), and does not match ignore patterns (`rnk-ignore`).
//! - Reaches context paths only through `rnk-rootfs`'s `RootPath`, and never
//!   writes to a local directory context.

mod source;
mod spec;
mod walk;

use std::path::PathBuf;

pub use source::{Context, ContextSource, DirSource, IgnoreFile, IgnoreSource, open};
pub use spec::{ContextSpec, parse_spec};
pub use walk::{Entry, EntryMeta, Walk, WalkStats};

/// The crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors from opening, reading or walking a build context.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// `--context` is empty.
    #[error("the build context is empty")]
    EmptySpec,
    /// `--context` has a prefix kaniko does not know either.
    #[error("unknown build context prefix {prefix:?}; use dir://, tar://, git:// or https://")]
    UnknownPrefix {
        /// The prefix, with `://`.
        prefix: String,
    },
    /// A context kind ranikust does not support (yet).
    #[error("{kind} build contexts are not supported: {reason}")]
    Unsupported {
        /// The kind, for example `tar`.
        kind: &'static str,
        /// Why, and when it arrives if planned.
        reason: &'static str,
    },
    /// The local context directory cannot be opened.
    #[error("build context directory {}: {source}", path.display())]
    Dir {
        /// The directory as given.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// The ignore file cannot be read.
    #[error("ignore file {}: {source}", path.display())]
    IgnoreFile {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// Reading a directory of the context failed.
    #[error("walking the build context at {}: {source}", path.display())]
    Walk {
        /// The path relative to the context root.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// A confined filesystem operation failed.
    #[error(transparent)]
    Rootfs(#[from] rnk_rootfs::Error),
    /// The ignore file or one of its patterns is malformed.
    #[error("ignore file: {0}")]
    Ignore(#[from] rnk_ignore::Error),
}
