//! OCI and Docker image types.
//!
//! Digests, media types, descriptors, manifests, indexes and image configs of
//! the OCI and Docker v2 schema 2 image formats, and their (de)serialization.
//! The types are owned here rather than taken from `oci-spec`: `oci-spec`
//! 0.10 keeps maps in `HashMap`s and drops the Docker-only config fields.
//!
//! # Boundaries
//!
//! - Types and (de)serialization only: no network or filesystem access, no
//!   clock, no randomness. Timestamps are inputs.
//! - Does not encode or compress layers (`rnk-layer`), move blobs
//!   (`rnk-registry`), or write a Docker tar or OCI layout (`rnk-builder`).

mod descriptor;
mod digest;
mod error;
mod media_type;

pub use descriptor::{Descriptor, Platform};
pub use digest::{Digest, DigestAlgorithm};
pub use error::Error;
pub use media_type::{Compression, ImageFormat, MediaType};
