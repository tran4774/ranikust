//! Stage builder: executes a stage's instructions and assembles the image.
//!
//! Builds a single stage: one executor per instruction (the `commands`
//! module), cache checks, lazy layer export (a layer is encoded only when it
//! is pushed, cached or part of the target), image config updates, then image
//! assembly and output (registry push, Docker tar, OCI layout).
//!
//! # Boundaries
//!
//! - No global state: paths arrive as a `BuildPaths` value from `rnk-store`.
//! - Does not order stages (`rnk-sched`) or parse flags (`ranikust`).
