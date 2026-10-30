//! What the scheduler runs: a stage, the stages folded into it, and where
//! its root is.

use rnk_dockerfile::{SquashedStage, StageGraph};

/// Where a stage's root is (ADR-0014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A root of its own: an overlay mount or a per-stage directory. Runs
    /// alongside other stages, bounded by `--jobs` and memory.
    Own,
    /// The shared root `/` of sequential kaniko mode: never runs while
    /// another shared stage does. Shared stages run one after the other in
    /// the order of the units, whatever order they became ready in.
    Shared,
}

/// One unit of a build: a stage, built after the stages folded into it,
/// in one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The stage whose output the unit produces.
    pub stage: usize,
    /// Stages run first in the same root, base first (kaniko's squashing,
    /// [`rnk_dockerfile::squash_for_shared_root`]); empty when nothing is
    /// folded. The unit waits for what these stages depend on, never for
    /// the folded stages themselves.
    pub folded: Vec<usize>,
    /// Where its root is.
    pub placement: Placement,
}

impl Unit {
    /// A stage on a root of its own, nothing folded in: every stage of an
    /// overlay-mode build.
    #[must_use]
    pub fn own(stage: usize) -> Self {
        Self {
            stage,
            folded: Vec::new(),
            placement: Placement::Own,
        }
    }

    /// The stages the unit runs, base first; the last is
    /// [`Unit::stage`].
    pub fn chain(&self) -> impl Iterator<Item = usize> + '_ {
        self.folded
            .iter()
            .copied()
            .chain(std::iter::once(self.stage))
    }
}

/// The units of an overlay-mode or per-stage-directory build: each needed
/// stage on its own root, in file order.
#[must_use]
pub fn own_root_units(graph: &StageGraph) -> Vec<Unit> {
    graph.needed.iter().copied().map(Unit::own).collect()
}

/// The units of a sequential kaniko-mode build: kaniko's squashed plan,
/// every unit on the shared root, in plan order.
#[must_use]
pub fn shared_root_units(plan: &[SquashedStage]) -> Vec<Unit> {
    plan.iter()
        .map(|s| Unit {
            stage: s.stage,
            folded: s.chain.iter().copied().filter(|&c| c != s.stage).collect(),
            placement: Placement::Shared,
        })
        .collect()
}
