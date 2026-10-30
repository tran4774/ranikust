//! Stage builder: executes a stage's instructions and assembles the image.
//!
//! Builds a single stage: one executor per instruction (the `commands`
//! module), cache checks, lazy layer export (a layer is encoded only when it
//! is pushed, cached or part of the target), image config updates, then image
//! assembly and output (registry push, Docker tar, OCI layout).
//!
//! Built so far: the output side. [`LayerExporter`] encodes a finished
//! layer ([`LayerSource`]) when its [`LayerDemand`] needs it; [`assemble`]
//! turns the final config and the [`BlobLayer`]s into an [`Image`] at one
//! [`BuildTime`]; [`write_oci_layout`], [`write_docker_tar`] and [`push`]
//! write it to the [`Outputs`].
//!
//! # Boundaries
//!
//! - No global state: paths arrive as a `BuildPaths` value from `rnk-store`
//!   (encoded layers go to its build directory), the clock and
//!   `SOURCE_DATE_EPOCH` as values.
//! - Does not order stages (`rnk-sched`) or parse flags (`ranikust`).
//! - Does not write tar itself: layers and the Docker tarball go through
//!   `rnk-layer`'s writer.
//! - Output paths come from the command line and are trusted; a path inside
//!   an image root is reached only through `rnk-rootfs`.

mod error;
mod image;
mod layer;
mod output;
mod time;

pub use error::{Error, Result};
pub use image::{HISTORY_AUTHOR, Image, assemble, image_format, layer_history};
pub use layer::{BlobLayer, EntriesLayer, LayerDemand, LayerExporter, LayerSource};
pub use output::{Outputs, UNSET_DESTINATION, push, write_docker_tar, write_oci_layout};
pub use time::BuildTime;
