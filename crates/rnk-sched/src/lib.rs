//! Stage scheduler: runs the stage DAG.
//!
//! Each stage is an async task that starts once its dependencies finish;
//! steps that do not touch a rootfs (base pulls, cache lookups) start for all
//! stages up front. `--jobs` bounds concurrent `RUN`s, available memory bounds
//! how many stages run at once, the first failure cancels the rest, and logs
//! are tagged per stage. In sequential kaniko mode the units are kaniko's
//! squashed stages, all on the shared `/` ([`Placement::Shared`]): they run
//! one at a time, in the order given, while stages with roots of their own
//! still run side by side ([`Scheduler::run_units`]).
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
mod unit;

pub use error::Error;
pub use memory::{MemoryCheck, MemoryGate, SystemMemory, Unlimited};
pub use runner::{LogTag, PrepareJob, StageJob, StageRunner};
pub use scheduler::{BuildSummary, Scheduler};
pub use slots::{RunPermit, RunSlots};
pub use tokio_util::sync::CancellationToken;
pub use unit::{Placement, Unit, own_root_units, shared_root_units};
