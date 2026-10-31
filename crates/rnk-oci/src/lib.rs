//! OCI and Docker image types.
//!
//! Digests, media types, descriptors, manifests, indexes and image configs of
//! the OCI and Docker v2 schema 2 image formats, and their (de)serialization;
//! image references, normalized as docker/distribution does; and the
//! canonical JSON ([`to_canonical_json`]) of everything ranikust writes, so
//! that the same value always has the same digest; the config edits of the
//! metadata instructions ([`ConfigEdit`]); and the index files of a
//! docker-save tarball and an OCI layout.
//!
//! The types are owned here rather than taken from `oci-spec`: `oci-spec`
//! 0.10 keeps maps in `HashMap`s and drops the Docker-only config fields.
//!
//! # Boundaries
//!
//! - Types and (de)serialization only: no network or filesystem access, no
//!   clock, no randomness. Timestamps are inputs.
//! - Does not encode or compress layers (`rnk-layer`), move blobs
//!   (`rnk-registry`), or write a Docker tar or OCI layout to disk
//!   (`rnk-builder`); it only names their files.
//! - Never re-serializes a pulled manifest for pushing: its digest covers the
//!   registry's exact bytes.

mod canonical;
mod config;
mod descriptor;
mod digest;
mod edit;
mod error;
mod layout;
mod manifest;
mod media_type;
mod reference;
mod serde_util;

pub use canonical::to_canonical_json;
pub use config::{ContainerConfig, HealthConfig, History, ImageConfig, RootFs};
pub use descriptor::{Descriptor, Platform};
pub use digest::{Digest, DigestAlgorithm};
pub use edit::{CommandLine, ConfigEdit, DEFAULT_SHELL, exposed_ports};
pub use error::Error;
pub use layout::{DockerTarImage, OciLayout};
pub use manifest::{ImageIndex, ImageManifest};
pub use media_type::{Compression, ImageFormat, MediaType};
pub use reference::Reference;

// The image-assembly types live here as pure data (Q-007, answered in
// `docs/open-questions.md`):
// `ConfigEdit` (what ENV, LABEL, USER… do to the config), history and
// `diff_ids` appends, the docker-save `manifest.json` (`DockerTarImage`) and
// the OCI layout's `oci-layout` (`OciLayout`); a layout's `index.json` is an
// `ImageIndex`. Moving them to `rnk-builder` would only change paths.
