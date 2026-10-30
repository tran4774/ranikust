//! Why a build stopped.

/// Why [`crate::Scheduler::run`] did not finish every stage. `E` is the
/// runner's error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error<E: std::error::Error + 'static> {
    /// The first stage to fail; every other stage was cancelled. A stage
    /// that fails after the cancellation is not reported.
    #[error("stage {name}: {source}")]
    Stage {
        /// The stage's index in the graph.
        stage: usize,
        /// Its display name.
        name: String,
        /// What the runner returned.
        #[source]
        source: E,
    },
    /// The up-front work of a stage (base pull, cache lookup) failed first.
    #[error("stage {name}: preparing: {source}")]
    Prepare {
        /// The stage's index in the graph.
        stage: usize,
        /// Its display name.
        name: String,
        /// What the runner returned.
        #[source]
        source: E,
    },
    /// The caller's cancellation token fired before any stage failed.
    #[error("build cancelled")]
    Cancelled,
    /// A needed stage depends on a stage outside `needed`: the graph is
    /// inconsistent, which `rnk_dockerfile::stage_graph` never produces.
    #[error("stage {stage} depends on stage {dependency}, which the targets do not need")]
    Graph {
        /// The stage.
        stage: usize,
        /// The dependency missing from `needed`.
        dependency: usize,
    },
}
