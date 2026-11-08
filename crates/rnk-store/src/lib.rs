//! Persistent content-addressed store, shared by builds.
//!
//! Owns the store in the state directory: compressed blobs (`blobs/`),
//! extracted layers (`layers/`), `RUN --mount=type=cache` directories
//! (`caches/`) and entries being written (`tmp/`). Provides per-digest locks,
//! publication by atomic rename, start-up clean-up and the emptying of
//! `ranikust prune --all` (ADR-0105; `prune` by size and age is Phase 5),
//! and defines `BuildPaths`, the paths handed down to every crate. Tells
//! whether the process runs in a user namespace other than the initial one
//! (by the inode of `/proc/self/ns/user`), for the group of its own
//! directories and for `rnk-snapshot`'s layer keys and device nodes.
//!
//! ```text
//! <state-dir>/
//! ├── .ranikust-state                marker: a non-empty directory without it is refused
//! ├── .ranikust-pruning              the marker while prune --all empties it: no build opens it
//! ├── blobs/<algorithm>/<hex>        compressed blobs, by digest
//! ├── layers/v3/<algorithm>/<hex>/   extracted layers, by chain ID [open: #1]
//! ├── caches/<sha256(id)>/           RUN --mount=type=cache
//! ├── locks/{blobs,layers/v3}/<algorithm>/<hex>   per-digest lock files [open: #1]
//! ├── tmp/<build-id>/<n>             entries being filled by one build
//! ├── builds/<build-id>/, <build-id>.lock      one per running build
//! ├── .ranikust-anon-userns-probe-*/ rootless overlay mode's probe, locked while in use: removed here
//! └── sandbox/                       the RUN sandbox's own files (made by the caller)
//! ```
//!
//! A build is alive while it holds the `flock` on `builds/<build-id>.lock`;
//! [`Store::open`] removes the `builds/` and `tmp/` directories of every
//! build that is not. [`Store::prune_all`] empties the whole directory only
//! while no build holds it, its marker renamed [`PRUNING_MARKER`] first,
//! which keeps every build out until a prune finishes.
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

mod anon;
mod error;
mod fence;
mod key;
mod lock;
mod store;
mod userns;

pub use anon::PREFIX as ANON_PREFIX;
pub use error::{Error, Result};
pub use fence::{NamedGuard, RunGuard};
pub use key::EntryKey;
pub use store::{
    BUILDING_MARKER, BuildPaths, CacheLease, CacheOwner, CacheSharing, MarkerKeeper,
    PRUNING_MARKER, PruneCheck, SANDBOX_DIR, STATE_MARKER, StateDirHold, Store, TmpEntry,
    USERNS_PROBE_PREFIX,
};
pub use userns::user_namespace_proc;
