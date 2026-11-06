//! The layer cache's interface: look an entry up, push a built layer under
//! one. Neither ever fails a build: a failure is a miss or a warning
//! (ADR-0113).

use rnk_oci::{Descriptor, Digest, Platform, Reference};
use rnk_registry::{BlobBody, PushOutcome};

use crate::error::Error;
use crate::key::EntryId;

/// Where cached layers live: a registry repository ([`RegistryCache`]) or,
/// later, an OCI layout.
///
/// Both methods return a value, never an error the caller could
/// propagate: a cache that fails costs a miss or a warning, and the
/// build's exit status and image do not change.
///
/// The futures are not `Send`, as the registry client's are not: drive
/// them with `Handle::block_on` on a thread of their own, as the builder
/// drives its uploads.
///
/// [`RegistryCache`]: crate::RegistryCache
pub trait LayerCache {
    /// Looks up `entry`: by its tag, then checked against its key, version
    /// and encoding and against `--cache-ttl`.
    fn lookup(&self, entry: &EntryId) -> impl Future<Output = Lookup>;

    /// Pushes `layer` under `entry`: the blob unless the cache holds it,
    /// then the entry's config and manifest. Runs beside the build.
    fn push(&self, entry: &EntryId, layer: &BuiltLayer) -> impl Future<Output = Pushed>;
}

/// What a lookup found.
#[derive(Debug)]
#[must_use]
#[expect(
    clippy::large_enum_variant,
    reason = "one per step, and a hit is the variant the caller keeps"
)]
pub enum Lookup {
    /// An entry of this key and encoding, younger than `--cache-ttl`.
    Hit(CachedLayer),
    /// No entry to use.
    Miss(Miss),
}

/// Why a lookup missed.
#[derive(Debug)]
pub enum Miss {
    /// The cache holds nothing under the tag.
    Absent,
    /// The entry is older than `--cache-ttl`: a plain miss, as in kaniko.
    Expired,
    /// The lookup failed or the entry was refused: a miss the caller warns
    /// about.
    Failed(Error),
}

/// A hit: the layer to apply instead of running the step.
///
/// Its tag proves nothing: the caller checks the blob against [`blob`]'s
/// digest and size and its tar against [`diff_id`] as it checks a base
/// layer's, and extracts it under the same rules (invariant 2 of #123); a
/// blob those rules refuse is found bad too. A blob found bad is a miss the
/// caller warns about, never a failed build (ADR-0113, ADR-0123), outside
/// ADR-0123's open cases (\[open: #171], \[open: #172]).
///
/// [`blob`]: CachedLayer::blob
/// [`diff_id`]: CachedLayer::diff_id
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedLayer {
    /// Where the blob is pulled from.
    pub source: Source,
    /// The compressed blob: the entry encoding's OCI media type, its digest
    /// and size, and nothing else the entry said.
    pub blob: Descriptor,
    /// The digest of the uncompressed tar.
    pub diff_id: Digest,
}

/// Where a hit's blob is.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Source {
    /// In this repository, tagged with the entry's tag.
    Registry(Reference),
}

/// A layer this build encoded, to push under its entry.
#[derive(Debug)]
pub struct BuiltLayer {
    /// The compressed blob. Its media type, OCI or Docker, must be of the
    /// entry's compression; the entry names it with the OCI type.
    pub blob: Descriptor,
    /// The digest of the uncompressed tar.
    pub diff_id: Digest,
    /// The platform the stage is built for, which the entry's config names
    /// (its architecture, OS and variant).
    pub platform: Platform,
    /// How to read the blob.
    pub body: BlobBody,
    /// A repository of the cache's registry that may hold the blob already
    /// (a destination it was pushed to): it is mounted from there rather
    /// than uploaded. Ignored on another registry.
    pub mount_from: Option<Reference>,
}

/// What a push did.
#[derive(Debug)]
#[must_use]
pub enum Pushed {
    /// The entry is in the cache; how its layer got there: already held,
    /// mounted or uploaded.
    Done(PushOutcome),
    /// The push failed: a warning, never a failed build.
    Failed(Error),
}
