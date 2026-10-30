//! Stage scheduler: runs the stage DAG.
//!
//! Each stage is an async task that starts once its dependencies finish;
//! steps that do not touch a rootfs (base pulls, cache lookups) start for all
//! stages up front. `--jobs` bounds concurrent `RUN`s, available memory bounds
//! how many stages run at once, a stage whose rootfs is the shared `/` runs
//! alone, the first failure cancels the rest, and logs are tagged per stage.
//!
//! How a stage is built is behind [`StageRunner`]: `rnk-builder` provides
//! it, tests use fakes. The scheduler drives every stage's future from the
//! one task that awaits [`Scheduler::run`]; a runner moves blocking work
//! (a `RUN`, extraction, compression) to its own threads.
//!
//! # Boundaries
//!
//! - Does not know how a stage is built (`rnk-builder`).
//! - Build output must never depend on execution order.

mod error;
mod memory;
mod runner;
mod scheduler;
mod slots;

pub use error::Error;
pub use memory::{MemoryCheck, MemoryGate, SystemMemory, Unlimited};
pub use runner::{LogTag, PrepareJob, StageJob, StageRunner};
pub use scheduler::{BuildSummary, Scheduler};
pub use slots::{RunPermit, RunSlots};
pub use tokio_util::sync::CancellationToken;
