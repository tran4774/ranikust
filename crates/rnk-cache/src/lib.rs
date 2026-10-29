//! Layer cache: cache keys, lookup and push.
//!
//! Computes the cache key of each instruction, and looks up and pushes cached
//! layers in a registry or an OCI layout.
//!
//! # Boundaries
//!
//! - Keys are ranikust's own and carry a format version; they are not
//!   compatible with kaniko's cache.
//! - Keys depend on the instruction, the context and the previous keys only,
//!   never on the snapshot mode.
//! - Does not own the local store: pulled cache layers enter it through
//!   `rnk-store`.
