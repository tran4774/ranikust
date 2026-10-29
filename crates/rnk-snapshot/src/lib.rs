//! Per-stage root filesystems and their per-instruction diffs.
//!
//! Defines the `StageRootfs` and `Snapshotter` traits and their two
//! implementations: overlay mode (overlay mounts, one upper dir per
//! instruction, overlay/OCI whiteout conversion, extraction of base layers
//! into the store's `layers/`) and kaniko mode (parallel walk and hash,
//! `LayeredMap`, path resolution, filesystem deletion). The `Snapshotter`
//! signature is still open (Q-024 in `docs/open-questions.md`); it is designed
//! at Phase 1 task 9.
//!
//! Also probes its own capabilities, which the binary runs at start-up: the
//! overlay mount with the ADR-0016 options, and `fsconfig` with `lowerdir+`.
//!
//! # Boundaries
//!
//! - Does not run instructions (`rnk-sandbox`) or encode diffs as layers: its
//!   exporter decides the entries and feeds them to `rnk-layer`'s writer.
//! - Does not own the store (`rnk-store`): it fills store entries and uses
//!   `layers/` as lower directories.
//! - Exposes a stage root as an `rnk-rootfs` `RootPath`, never as a bare host
//!   path.
