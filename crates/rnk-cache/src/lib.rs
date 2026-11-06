//! Layer cache: cache keys, lookup and push.
//!
//! Computes the versioned cache key of each step (ADR-0005, [ADR-0113]),
//! and looks up and pushes the layer cached under it in a registry
//! repository ([`RegistryCache`]); a cache in an OCI layout is planned.
//!
//! A chain starts at [`CacheKey::root`]; each step's key is
//! [`CacheKey::next`] of the previous one, over the step's expanded
//! instruction and its [`KeyInputs`]. An [`EntryId`] pairs a key with the
//! layer's [`Encoding`]: the tag, the entry's annotations and its layer
//! type all come from it, so a hit was encoded with this build's settings.
//! File content is hashed by a [`ContentHasher`] from bytes the caller
//! reads. A [`LayerCache`] lookup or push returns a value, never an error:
//! a cache that fails is a miss or a warning.
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
//!   a registry only through `rnk-registry`'s client, with a destination's
//!   credentials, trust and `--limit` bounds ([ADR-0115]).
//! - A lookup or push never fails the build or changes its image: a
//!   failure is a miss or a warning. A hit is not trusted: the caller
//!   verifies its blob and diff ID and extracts it as a base layer's.
//! - Depends on `rnk-oci` for digests, platforms and the entry's documents
//!   and on `rnk-registry` for transfers, never on `rnk-dockerfile`,
//!   `rnk-ignore` or `rnk-layer`: `rnk-builder` builds [`KeyInputs`] from
//!   parsed instructions and the context walk ([ADR-0115]). The instruction
//!   holds no heredoc body: `rnk-builder` adds each one (of a `RUN`, `#!` or
//!   not, a `COPY` or an `ADD`) through a `KeyInputs` method that task 3
//!   (#126) adds.
//! - Does not own the local store: pulled cache layers enter it through
//!   `rnk-store`.
//!
//! [ADR-0113]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md
//! [ADR-0115]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md

mod cache;
mod canonical;
mod entry;
mod error;
mod inputs;
mod key;
mod registry;

pub use cache::{BuiltLayer, CachedLayer, LayerCache, Lookup, Miss, Pushed, Source};
pub use entry::{ENCODING_ANNOTATION, KEY_ANNOTATION, VERSION_ANNOTATION};
pub use error::{Error, Refusal};
pub use inputs::{
    Content, ContentHasher, CopyFrom, File, FileKind, KeyInputs, Mount, Network, Sharing,
};
pub use key::{CacheKey, Encoding, EntryId, KEY_VERSION, TimePolicy};
pub use registry::RegistryCache;
