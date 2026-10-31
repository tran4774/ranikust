//! Per-stage root filesystems and their per-instruction diffs.
//!
//! Defines the [`Snapshotter`] and [`StageRootfs`] traits (ADR-0046) and
//! their overlay implementation,
//! [`Overlay`]: base layers extracted into the store's `layers/` in the
//! overlay form, one overlay mount per layer with a fresh upper directory
//! (`ADR-0016` options), and the exporter that turns an upper directory
//! into an OCI layer through `rnk-layer`'s writer ([`LayerDiff`]). kaniko
//! mode's walk is [`Walker`]: a parallel walk and hash of a root, diffed
//! against the previous walk while walking ([`WalkDiff`], ADR-0039), and
//! [`WalkDiff::export`], which writes that diff as a layer through the same
//! writer; its stage roots on the one shared root, [`Kaniko`] and
//! [`KanikoRoot`], implement the same traits.
//!
//! Also probes its own capability, which the binary runs at start-up:
//! [`probe`], the overlay mount with the ADR-0016 options, with
//! `fsconfig(lowerdir+)` or else `mount(2)`.
//!
//! # Boundaries
//!
//! - Does not run instructions (`rnk-sandbox`) or encode diffs as layers: its
//!   exporter decides the entries and feeds them to `rnk-layer`'s writer.
//! - Does not own the store (`rnk-store`): it fills store entries and uses
//!   `layers/` as lower directories.
//! - Exposes a stage root as an `rnk-rootfs` `RootPath`, never as a bare host
//!   path; the one host path it hands out is the mount point, for the
//!   sandbox to enter.

mod error;
mod export;
pub mod kaniko;
mod layer;
mod lowers;
mod mount;
mod probe;
mod stage;

pub use error::{Error, Result};
pub use export::{DiffCounts, LayerDiff};
pub use kaniko::{
    Change, Changed, DEFAULT_WALK_WORKERS, EntryMeta, Kaniko, KanikoDiff, KanikoLayer, KanikoRoot,
    SnapshotMode, WalkDiff, WalkOptions, WalkStats, Walker,
};
pub use layer::LayerDir;
pub use mount::{MAX_LOWERS, MountApi, OVERLAY_OPTIONS};
pub use probe::{OverlayCaps, probe};
pub use stage::{Base, ExportLayer, Keep, Overlay, OverlayStage, Snapshotter, StageRootfs};
