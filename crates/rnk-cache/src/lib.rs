//! Layer cache: cache keys, lookup and push.
//!
//! Computes the versioned cache key of each step (ADR-0005, ADR-0113) and
//! the tag its cached layer is looked up and pushed under; lookup and push
//! in a registry or an OCI layout are not built yet.
//!
//! A chain starts at [`CacheKey::root`]; each step's key is
//! [`CacheKey::next`] of the previous one, over the step's expanded
//! instruction and its [`KeyInputs`]. [`CacheKey::tag`] adds the layer's
//! [`Encoding`], so a hit was encoded with this build's settings. File
//! content is hashed by a [`ContentHasher`] from bytes the caller reads.
//!
//! # Boundaries
//!
//! - Keys are ranikust's own and carry a format version, [`KEY_VERSION`];
//!   they are not compatible with kaniko's cache (ADR-0005).
//! - A key depends on the step's instruction, its inputs and the previous
//!   key only: never on the snapshot mode, a host user or id map, the
//!   build's time, an mtime outside `--reproducible`, a thread count or the
//!   order of an unordered collection. The API takes no snapshot mode,
//!   host user or id map, clock time or thread count, and has no field
//!   meant for a secret's value; callers never pass a secret mount's `env=`
//!   variable to [`KeyInputs::env`] (threat model T11).
//! - Key computation opens no file and no connection: file content arrives
//!   as bytes the caller read through `rnk-rootfs`. The cache in an OCI
//!   layout (planned) reaches its directory only through `rnk-rootfs`, and
//!   a registry only through `rnk-registry` (ADR-0115).
//! - Depends on `rnk-oci` for digests and platforms, never on
//!   `rnk-dockerfile`, `rnk-ignore` or `rnk-layer`: `rnk-builder` builds
//!   [`KeyInputs`] from parsed instructions and the context walk
//!   (ADR-0115).
//! - Does not own the local store: pulled cache layers enter it through
//!   `rnk-store`.

mod canonical;
mod inputs;
mod key;

pub use inputs::{
    Content, ContentHasher, CopyFrom, File, FileKind, KeyInputs, Mount, Network, Sharing,
};
pub use key::{CacheKey, Encoding, KEY_VERSION, TimePolicy};
