//! OCI and Docker image types.
//!
//! Manifests, image configs and media types of the OCI and Docker image
//! formats, built on `oci-spec`, and the canonical JSON serialization, with
//! sorted keys, of every config and manifest ranikust writes: `oci-spec`
//! serializes some maps from `HashMap`s, whose order would change the digest.
//!
//! # Boundaries
//!
//! - Types and (de)serialization only: no network or filesystem access.
