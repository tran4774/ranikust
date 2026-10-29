//! `.dockerignore` matching.
//!
//! Parses `.dockerignore` contents and decides whether a context path is
//! excluded, with exactly the semantics of moby's `patternmatcher` v0.6.1
//! as BuildKit v0.34.0 and the docker CLI use it, built with Go 1.26:
//! pattern syntax, `!` exceptions and parent-directory matches. This is not
//! gitignore: a pattern without `/` matches only at the root.
//!
//! ```
//! let patterns = rnk_ignore::read_patterns(b"# build output\n*.log\ndocs\n!docs/keep.md\n")?;
//! let matcher = rnk_ignore::Matcher::new(&patterns)?;
//! assert!(matcher.matches_or_parent_matches("app.log")?);
//! assert!(!matcher.matches_or_parent_matches("sub/app.log")?);
//! assert!(matcher.matches_or_parent_matches("docs/drop.md")?);
//! assert!(!matcher.matches_or_parent_matches("docs/keep.md")?);
//! // `docs` is excluded, but the walk must look inside it for `keep.md`.
//! assert!(matcher.may_reinclude_below("docs"));
//! # Ok::<(), rnk_ignore::Error>(())
//! ```
//!
//! Strings are bytes, as in Go: patterns and paths may hold any byte, and
//! where Go decodes UTF-8 an invalid byte reads as U+FFFD. The Go behaviour
//! this depends on (`filepath.Clean`, `filepath.Match`'s validation,
//! `regexp` syntax and Unicode classes) is ported too, not approximated by
//! the `regex` crate's own syntax.
//!
//! # Boundaries
//!
//! - Matches paths only; never reads or walks the filesystem, and does not
//!   choose the ignore file (`rnk-context`).
//! - No approximations: any divergence from `patternmatcher` is a bug.

mod golang;
mod goregexp;
mod ignorefile;
mod matcher;
mod unicode_tables;

pub use ignorefile::read_patterns;
pub use matcher::{MatchInfo, Matcher};

/// Errors from reading or matching `.dockerignore` patterns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A line of the ignore file is 64 KiB or longer, which Go's
    /// `bufio.Scanner` refuses ("token too long").
    #[error("line {line} of the ignore file is too long (65,535 bytes at most)")]
    LineTooLong {
        /// The line number, from 1.
        line: usize,
    },
    /// A pattern is a lone `!`.
    #[error("illegal exclusion pattern: \"!\"")]
    IllegalExclusion,
    /// A pattern is malformed (Go's `filepath.ErrBadPattern`), either as
    /// `filepath.Match` reads it or as the regular expression it becomes.
    #[error("syntax error in pattern {pattern:?}")]
    BadPattern {
        /// The cleaned pattern, without its `!`; invalid UTF-8 replaced.
        pattern: String,
    },
    /// The [`MatchInfo`] passed to [`Matcher::matches_using_parent`] was
    /// returned by a matcher with a different number of patterns.
    #[error("match info from another matcher")]
    MatchInfoMismatch,
}
