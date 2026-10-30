//! What the scheduler asks of whoever builds a stage.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use rnk_dockerfile::{StageGraph, StageNode};
use tokio_util::sync::CancellationToken;

use crate::slots::{RunPermit, RunSlots};

/// Builds stages for the scheduler: `rnk-builder` in a build, fakes in
/// tests.
///
/// The scheduler calls [`prepare`](Self::prepare) for every needed stage at
/// the start of the build, then [`build`](Self::build) for a stage once its
/// own preparation and every stage it depends on have finished. Both run on
/// the task that awaits [`crate::Scheduler::run`]: blocking work belongs on
/// the runner's own threads. Both should return soon after their token
/// fires; the scheduler waits for every call to end before it returns,
/// since clean-up must not rely on `Drop`.
pub trait StageRunner: Sync {
    /// What up-front work hands to the stage's build (a pulled base).
    type Prepared: Send;
    /// What a finished stage hands to the stages that depend on it (its
    /// final root, its config) and to the caller.
    type Output: Send + Sync;
    /// Why a stage failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Work that touches no root: resolve and pull the base, look up the
    /// cache. Runs for all stages at once.
    fn prepare(
        &self,
        job: PrepareJob<'_>,
    ) -> impl Future<Output = Result<Self::Prepared, Self::Error>> + Send;

    /// Builds one stage, its instructions in order. Takes a `--jobs` permit
    /// per `RUN` with [`StageJob::run_slot`].
    fn build(
        &self,
        job: StageJob<'_, Self>,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
}

/// The up-front work of one stage.
#[derive(Debug)]
pub struct PrepareJob<'a> {
    /// The stage's index in the graph.
    pub stage: usize,
    /// The stage.
    pub node: &'a StageNode,
    /// The whole graph.
    pub graph: &'a StageGraph,
    /// The stage's log tag.
    pub tag: LogTag,
    /// Fires when the build is cancelled.
    pub cancel: CancellationToken,
}

/// One stage to build.
///
/// Which layers of the stage must be encoded (`LayerDemand`) is not
/// computed here: [open: Q-029] decides whether the scheduler or the
/// builder derives it; `graph` and `stage` are enough for either.
pub struct StageJob<'a, R: StageRunner + ?Sized> {
    /// The stage's index in the graph.
    pub stage: usize,
    /// The stage.
    pub node: &'a StageNode,
    /// The whole graph.
    pub graph: &'a StageGraph,
    /// The outputs of the stages it depends on, in
    /// [`StageNode::dependencies`] order: the base stage first.
    pub deps: Vec<(usize, Arc<R::Output>)>,
    /// What [`StageRunner::prepare`] returned for this stage.
    pub prepared: R::Prepared,
    /// The stage's log tag.
    pub tag: LogTag,
    /// Fires when the build is cancelled.
    pub cancel: CancellationToken,
    pub(crate) slots: &'a RunSlots,
}

impl<'a, R: StageRunner + ?Sized> StageJob<'a, R> {
    /// Waits for a `--jobs` permit for one `RUN`; `None` once the build is
    /// cancelled.
    pub async fn run_slot(&self) -> Option<RunPermit<'a>> {
        self.slots.acquire(&self.cancel).await
    }

    /// The shared `--jobs` permits.
    #[must_use]
    pub fn slots(&self) -> &'a RunSlots {
        self.slots
    }
}

impl<R: StageRunner + ?Sized> fmt::Debug for StageJob<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StageJob")
            .field("stage", &self.stage)
            .field("tag", &self.tag)
            .field(
                "deps",
                &self.deps.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The prefix of a stage's log lines: `[<stage-name> <k>/<n>]`, `k` its
/// position among the stages the build runs (from 1, file order), `n`
/// their number.
// [open: Q-031] grouping output per stage, and how it meets kaniko's log
// flags, is undecided; the tag is the same either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTag {
    /// The stage's display name.
    pub name: String,
    /// Its position, from 1.
    pub k: usize,
    /// Stages in the build.
    pub n: usize,
}

impl fmt::Display for LogTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} {}/{}]", self.name, self.k, self.n)
    }
}
