// Derived from kaniko, https://github.com/osscontainertools/kaniko,
// pkg/dockerfile/dockerfile.go at commit 44aa7ddc0 (MakeKanikoStages,
// lines 297-415, and Squash, lines 482-497).
// Copyright 2018 Google LLC. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: works on the resolved stage graph instead of
// kaniko's stage list, counts `RUN --mount from=` as `COPY --from`, and
// adds which stages need their layers (ADR-0038) and what later stages read.

//! The plan of a sequential kaniko-mode build, where every stage runs in
//! the one shared root `/`: kaniko's stage squashing, which stages are
//! walked for layers (lazy snapshots), and what each stage must leave behind
//! before the root is emptied.
//!
//! Only sequential kaniko mode squashes. Overlay mode and per-stage
//! directories build every stage on its own root, so they need no
//! squashing; [`layers_needed`] gives their lazy-snapshot rule.

use crate::graph::{Base, Source, StageGraph};

/// One unit of a shared-root build: a stage, with the base stages kaniko
/// folded into it run first in the same root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquashedStage {
    /// The stage whose image this unit builds: its name, its place among
    /// the targets and its index in `COPY --from` stay this stage's, as
    /// kaniko's `Squash` keeps the child's.
    pub stage: usize,
    /// The stages run one after the other in the root, base first; the last
    /// is [`SquashedStage::stage`]. Each runs its `ONBUILD` triggers and then
    /// its instructions; every stage but the last runs without its own
    /// `ONBUILD` instructions, which kaniko's `Squash` drops (their
    /// triggers already run in the next stage of the chain).
    pub chain: Vec<usize>,
    /// What the chain starts from: the base of its first stage. A
    /// [`Base::Stage`] here is a unit of the plan, built earlier, whose
    /// image [`SquashedStage::save_image`] keeps.
    pub base: Base,
    /// A later unit builds `FROM` this one: its image (config and layers)
    /// must be kept before the root is emptied. kaniko's `SaveStage`,
    /// without its mark on the pushed stage.
    pub save_image: bool,
    /// The unit is walked and its layers recorded; when false it runs
    /// without any snapshot (ADR-0038).
    pub layers_needed: bool,
    /// The references that read this unit's root from later units, as
    /// `(stage, index in that stage's references)`: `COPY --from` sources
    /// to keep, and `RUN --mount from=` roots, before the root is emptied.
    /// In plan order, without duplicates.
    pub readers: Vec<(usize, usize)>,
}

/// kaniko's stage squashing for a build whose stages share one root, over
/// the stages the targets need (`graph.needed`), in file order.
///
/// A stage built `FROM` an earlier stage absorbs it when nothing else uses
/// that base: exactly one needed stage builds on it, no `COPY --from` or
/// `RUN --mount from=` reads it, and it is not the result. The base then
/// runs first in the child's root, and no image of it is kept. Chains fold
/// whole. A base that is also a `--target` is folded into its child and
/// still built on its own, as kaniko does. A needed stage that is not a
/// target and that no later unit uses is dropped.
///
/// Layers (`ADR-0038`, Q-034): a unit is walked when it is the result, or
/// when a walked unit is built `FROM` it. A unit whose image is kept for a
/// later `FROM` is walked as well, since the shared root can only be
/// rebuilt from that image (ADR-0044).
///
/// Order and contents depend on the graph only.
#[must_use]
pub fn squash_for_shared_root(graph: &StageGraph) -> Vec<SquashedStage> {
    let n = graph.stages.len();
    let base_of = |i: usize| graph.stages.get(i).map(|s| &s.base);
    // kaniko counts, per stage, the needed stages built on it
    // (`stagesDependencies`, with one extra for the pushed stage so it is
    // never folded) and the commands reading it (`copyDependencies`).
    let mut base_users = vec![0usize; n];
    let mut readers = vec![0usize; n];
    let primary = graph.primary_target();
    if let Some(u) = base_users.get_mut(primary) {
        *u += 1;
    }
    for &i in &graph.needed {
        if let Some(Base::Stage(b)) = base_of(i)
            && let Some(u) = base_users.get_mut(*b)
        {
            *u += 1;
        }
        for r in graph.stages.get(i).map_or(&[][..], |s| &s.references) {
            if let Source::Stage(s) = r.source
                && let Some(c) = readers.get_mut(s)
            {
                *c += 1;
            }
        }
    }

    // Fold in file order, so a chain folds whole: by the time a child is
    // visited, its base already holds its own folded chain.
    let mut chains: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    for &i in &graph.needed {
        let Some(&Base::Stage(b)) = base_of(i) else {
            continue;
        };
        if base_users.get(b) == Some(&1) && readers.get(b) == Some(&0) {
            let mut chain = chains.get(b).cloned().unwrap_or_default();
            chain.push(i);
            if let Some(c) = chains.get_mut(i) {
                *c = chain;
            }
            if let Some(u) = base_users.get_mut(b) {
                *u = 0;
            }
        }
    }

    let kept: Vec<usize> = graph
        .needed
        .iter()
        .copied()
        .filter(|&i| {
            graph.targets.contains(&i)
                || base_users.get(i).is_some_and(|&u| u > 0)
                || readers.get(i).is_some_and(|&c| c > 0)
        })
        .collect();
    let mut plan: Vec<SquashedStage> = kept
        .iter()
        .map(|&i| {
            let chain = chains.get(i).cloned().unwrap_or_else(|| vec![i]);
            let first = chain.first().copied().unwrap_or(i);
            SquashedStage {
                stage: i,
                base: base_of(first).cloned().unwrap_or(Base::Scratch),
                chain,
                save_image: false,
                layers_needed: false,
                readers: Vec::new(),
            }
        })
        .collect();

    let unit_of = |stage: usize| kept.iter().position(|&k| k == stage);
    for k in 0..plan.len() {
        let (base, chain) = (plan[k].base.clone(), plan[k].chain.clone());
        if let Base::Stage(b) = base
            && let Some(u) = unit_of(b)
        {
            plan[u].save_image = true;
        }
        for c in chain {
            let refs = graph.stages.get(c).map_or(&[][..], |s| &s.references);
            for (ri, r) in refs.iter().enumerate() {
                if let Source::Stage(s) = r.source
                    && let Some(u) = unit_of(s)
                    && !plan[u].readers.contains(&(c, ri))
                {
                    plan[u].readers.push((c, ri));
                }
            }
        }
    }

    // Walked units: the result, every saved image, and what they are built
    // `FROM`, transitively. Bases come earlier, so one pass from the end
    // reaches every one.
    for k in (0..plan.len()).rev() {
        let unit = &plan[k];
        if !(unit.stage == primary || unit.save_image || unit.layers_needed) {
            continue;
        }
        plan[k].layers_needed = true;
        if let Base::Stage(b) = plan[k].base
            && let Some(u) = unit_of(b)
        {
            plan[u].layers_needed = true;
        }
    }
    plan
}

/// The lazy-snapshot rule of `ADR-0038` (Q-034) where every stage has its
/// own root (per-stage directories): the stages whose layers are needed,
/// in file order. The result needs its layers, and so does every stage a
/// stage that needs its layers builds `FROM`. A stage only read through
/// `COPY --from` or `RUN --mount from=` is not walked.
#[must_use]
pub fn layers_needed(graph: &StageGraph) -> Vec<usize> {
    let mut needed = vec![false; graph.stages.len()];
    let mut next = Some(graph.primary_target());
    while let Some(i) = next {
        match needed.get_mut(i) {
            Some(seen) if !*seen => *seen = true,
            _ => break,
        }
        next = match graph.stages.get(i).map(|s| &s.base) {
            Some(Base::Stage(b)) => Some(*b),
            _ => None,
        };
    }
    graph
        .needed
        .iter()
        .copied()
        .filter(|&i| needed.get(i) == Some(&true))
        .collect()
}

#[cfg(test)]
mod tests;
