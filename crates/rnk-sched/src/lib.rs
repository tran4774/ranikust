//! Stage scheduler: runs the stage DAG.
//!
//! Each stage is an async task that starts once its dependencies finish;
//! steps that do not touch a rootfs (base pulls, cache lookups) start for all
//! stages up front. `--jobs` bounds concurrent `RUN`s, available memory bounds
//! how many stages run at once, a stage whose rootfs is the shared `/` runs
//! alone, the first failure cancels the rest, and logs are tagged per stage.
//!
//! # Boundaries
//!
//! - Does not know how a stage is built (`rnk-builder`).
//! - Build output must never depend on execution order.
