//! `.dockerignore` matching.
//!
//! Parses `.dockerignore` contents and decides whether a context path is
//! excluded, with exactly the semantics of moby's `patternmatcher`: pattern
//! syntax, `!` exceptions and parent-directory matches.
//!
//! # Boundaries
//!
//! - Matches paths only; never reads or walks the filesystem, and does not
//!   choose the ignore file (`rnk-context`).
//! - No approximations: any divergence from `patternmatcher` is a bug.
