//! Stage builder: executes a stage's instructions and assembles the image.
//!
//! Builds a single stage: [`build_stage`] runs one executor per instruction
//! on a stage root from `rnk-snapshot`, `RUN` through `rnk-sandbox`, with lazy
//! layer export (a layer is encoded only when it is pushed, cached or part of
//! the target); [`pull_image`] fills the store with a base image. Then image
//! assembly and output: [`LayerExporter`] encodes a finished layer
//! ([`LayerSource`]) when its [`LayerDemand`] needs it; [`assemble`] turns the
//! final config and the [`BlobLayer`]s into an [`Image`] at one
//! [`BuildTime`]; [`write_oci_layout`], [`write_docker_tar`] and [`push`]
//! write it to the [`Outputs`]. Cache checks come in Phase 4.
//!
//! # Boundaries
//!
//! - No global state: every path is a value from the caller (the store and
//!   its build directory, the context, the secrets), and so are the clock
//!   and `SOURCE_DATE_EPOCH`.
//! - Does not depend on `rnk-sched`: `ranikust` implements its
//!   `StageRunner` with [`build_stage`].
//! - Does not order stages (`rnk-sched`) or parse flags (`ranikust`).
//! - Does not write tar itself: layers and the Docker tarball go through
//!   `rnk-layer`'s writer.
//! - Output paths come from the command line and are trusted; a path inside
//!   an image root is reached only through `rnk-rootfs`.

mod error;
mod glob;
mod history;
mod image;
mod layer;
mod mounts;
mod output;
mod pull;
mod sources;
mod stage;
mod time;
mod upload;

pub use error::{Error, Result};
pub use image::{
    HISTORY_COMMENT, Image, assemble, assemble_annotated, image_format, instruction_history,
};
pub use layer::{
    AnonBlob, BlobLayer, EntriesLayer, LayerBytes, LayerDemand, LayerExporter, LayerSource,
};
pub use mounts::{MAX_SECRET_SIZE, Secret};
pub use output::{Outputs, UNSET_DESTINATION, push, write_docker_tar, write_oci_layout};
pub use pull::{PulledImage, pull_image};
pub use sources::ContextFiles;
pub use stage::{
    BuildCx, ImageSource, KeepNeed, RunGate, SourceRoot, StageBase, StageDone, StageInput, Ungated,
    build_stage,
};
pub use time::BuildTime;
pub use upload::{Release, Uploader};
