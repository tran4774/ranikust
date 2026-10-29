//! Build context sources.
//!
//! Provides the build context from a local directory, a tar archive, a git
//! repository or an HTTPS URL, behind an open trait so that new sources can be
//! added later. Locates the ignore file: `<Dockerfile>.dockerignore` takes
//! precedence over `.dockerignore` at the context root, as in kaniko and
//! BuildKit.
//!
//! # Boundaries
//!
//! - No cloud object-store sources (S3, GCS, Azure) for now.
//! - Does not interpret the Dockerfile or decide which files an instruction
//!   copies (`rnk-builder`), and does not match ignore patterns (`rnk-ignore`).
