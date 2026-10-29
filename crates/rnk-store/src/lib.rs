//! Persistent content-addressed store, shared by builds.
//!
//! Owns the store in the state directory: compressed blobs (`blobs/`),
//! extracted layers (`layers/`), `RUN --mount=type=cache` directories
//! (`caches/`) and entries being written (`tmp/`). Provides per-digest locks,
//! publication by atomic rename, start-up clean-up and `prune`, and defines
//! `BuildPaths`, the paths handed down to every crate.
//!
//! # Boundaries
//!
//! - Does not decide what an entry contains: the caller fills a temporary
//!   entry (`rnk-registry` a blob, `rnk-snapshot` an extracted layer) and
//!   this crate publishes it.
//! - Does not transfer blobs (`rnk-registry`), decode or encode layers
//!   (`rnk-layer`), mount or diff (`rnk-snapshot`), or compute cache keys
//!   (`rnk-cache`).
//! - Writes only inside the state directory, and deletes only through
//!   `rnk-rootfs`: entries hold untrusted image content.
