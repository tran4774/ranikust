//! Persistent content-addressed store, shared by builds.
//!
//! Owns the store in the state directory: compressed blobs (`blobs/`),
//! extracted layers (`layers/`), `RUN --mount=type=cache` directories
//! (`caches/`) and entries being written (`tmp/`). Provides per-digest locks,
//! publication by atomic rename, start-up clean-up and `prune`, and defines
//! `BuildPaths`, the paths handed down to every crate.
//!
//! ```text
//! <state-dir>/
//! ├── .ranikust-root                 marker: a non-empty directory without it is refused
//! ├── blobs/<algorithm>/<hex>        compressed blobs, by digest
//! ├── layers/<algorithm>/<hex>/      extracted layers, by chain ID [open: Q-015]
//! ├── caches/<sha256(id)>/           RUN --mount=type=cache
//! ├── locks/{blobs,layers}/<algorithm>/<hex>   per-digest lock files [open: Q-015]
//! ├── tmp/<build-id>/<n>             entries being filled by one build
//! └── builds/<build-id>/, <build-id>.lock      one per running build
//! ```
//!
//! A build is alive while it holds the `flock` on `builds/<build-id>.lock`;
//! [`Store::open`] removes the `builds/` and `tmp/` directories of every
//! build that is not.
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

mod error;
mod key;
mod lock;
mod store;

pub use error::{Error, Result};
pub use key::EntryKey;
pub use store::{BuildPaths, Store, TmpEntry};
