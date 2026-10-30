use super::*;
use crate::graph::{GraphOptions, stage_graph};
use crate::instructions::parse;
use crate::scope::Platform;

fn graph(src: &str, targets: &[&str]) -> StageGraph {
    let opts = GraphOptions {
        build_args: std::collections::BTreeMap::new(),
        build_platform: Platform::new("linux", "amd64"),
        target_platform: Platform::new("linux", "amd64"),
        targets: targets.iter().map(|t| (*t).to_owned()).collect(),
    };
    stage_graph(&parse(src).unwrap_or_else(|e| panic!("{e}")), &opts)
        .unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

/// `(stage, chain, save_image, layers_needed)` per unit.
fn shape(plan: &[SquashedStage]) -> Vec<(usize, Vec<usize>, bool, bool)> {
    plan.iter()
        .map(|u| (u.stage, u.chain.clone(), u.save_image, u.layers_needed))
        .collect()
}

/// Every needed stage runs in some unit, and units are in file order.
fn check_covers(g: &StageGraph, plan: &[SquashedStage]) {
    for &i in &g.needed {
        assert!(
            plan.iter().any(|u| u.chain.contains(&i)),
            "stage {i} runs nowhere: {plan:?}"
        );
    }
    assert!(plan.windows(2).all(|w| w[0].stage < w[1].stage));
    for u in plan {
        assert_eq!(u.chain.last(), Some(&u.stage));
    }
}

#[test]
fn a_chain_of_single_children_folds_whole() {
    let g = graph(
        "FROM alpine AS a\nRUN one\nFROM a AS b\nRUN two\nFROM b AS c\nRUN three\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    assert_eq!(shape(&plan), [(2, vec![0, 1, 2], false, true)]);
    assert_eq!(plan[0].base, Base::Image("alpine".to_owned()));
    assert!(plan[0].readers.is_empty());
}

#[test]
fn a_base_also_read_by_copy_from_is_not_folded() {
    // kaniko's "args" case of `TestCalculateDependencies`.
    let g = graph(
        "FROM debian AS stage1\nRUN foo\nFROM stage1\nCOPY --from=stage1 /tmp/x.txt .\nRUN bar\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    // The image of stage1 is kept for the FROM, so it is walked
    // ([open: Q-300]).
    assert_eq!(
        shape(&plan),
        [(0, vec![0], true, true), (1, vec![1], false, true)]
    );
    assert_eq!(plan[0].readers, [(1, 0)]);
    assert_eq!(plan[1].base, Base::Stage(0));
}

#[test]
fn a_base_used_by_two_stages_is_kept_whole() {
    let g = graph(
        "FROM alpine AS base\nRUN a\n\
         FROM base AS tool\nRUN b\n\
         FROM base\nCOPY --from=tool /t /t\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    assert_eq!(
        shape(&plan),
        [
            (0, vec![0], true, true),
            // Only read by COPY --from: not walked.
            (1, vec![1], false, false),
            (2, vec![2], false, true),
        ]
    );
    assert_eq!(plan[1].readers, [(2, 0)]);
}

#[test]
fn a_copy_source_is_not_walked() {
    // kaniko's "simple deps" case.
    let g = graph(
        "FROM debian AS stage1\nRUN make\nFROM alpine\nCOPY --from=stage1 /foo /bar\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    assert_eq!(
        shape(&plan),
        [(0, vec![0], false, false), (1, vec![1], false, true)]
    );
    assert_eq!(plan[0].readers, [(1, 0)]);
}

#[test]
fn the_pushed_stage_is_never_folded() {
    let g = graph(
        "FROM alpine AS a\nRUN one\nFROM a AS b\nRUN two\n",
        &["a", "b"],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    // `a` is the result and `b` builds on it: two units, `a`'s image kept.
    assert_eq!(
        shape(&plan),
        [(0, vec![0], true, true), (1, vec![1], false, false)]
    );
}

#[test]
fn a_base_that_is_another_target_is_folded_and_still_built() {
    let g = graph(
        "FROM alpine AS a\nRUN one\nFROM a AS b\nRUN two\n",
        &["b", "a"],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    assert_eq!(
        shape(&plan),
        [(0, vec![0], false, false), (1, vec![0, 1], false, true)]
    );
}

#[test]
fn a_mount_from_blocks_folding_like_copy_from() {
    let g = graph(
        "FROM alpine AS a\nRUN one\nFROM a\nRUN --mount=from=a,target=/x true\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    assert_eq!(
        shape(&plan),
        [(0, vec![0], true, true), (1, vec![1], false, true)]
    );
    assert_eq!(plan[0].readers, [(1, 0)]);
}

#[test]
fn onbuild_triggers_count_as_readers_of_the_child() {
    let g = graph(
        "FROM alpine AS src\nRUN make\n\
         FROM alpine AS base\nONBUILD COPY --from=src /a /a\n\
         FROM base\nRUN use\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    // `base` folds into its only child; the trigger, run in the child,
    // reads `src`.
    assert_eq!(
        shape(&plan),
        [(0, vec![0], false, false), (2, vec![1, 2], false, true)]
    );
    assert_eq!(plan[0].readers, [(2, 0)]);
    assert_eq!(plan[1].base, Base::Image("alpine".to_owned()));
}

#[test]
fn a_saved_base_and_its_own_bases_are_walked() {
    // `mid` is folded into nothing: two stages build on it. Its chain from
    // `root` folds, and the kept image needs the layers of the whole chain.
    let g = graph(
        "FROM alpine AS root\nRUN r\n\
         FROM root AS mid\nRUN m\n\
         FROM mid AS x\nRUN x\n\
         FROM mid AS y\nCOPY --from=x /x /x\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    check_covers(&g, &plan);
    assert_eq!(
        shape(&plan),
        [
            (1, vec![0, 1], true, true),
            (2, vec![2], false, false),
            (3, vec![3], false, true),
        ]
    );
}

#[test]
fn stages_no_target_needs_are_not_planned() {
    let g = graph(
        "FROM alpine AS unused\nRUN x\nFROM scratch\nCOPY a b\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    assert_eq!(shape(&plan), [(1, vec![1], false, true)]);
}

#[test]
fn per_stage_roots_walk_the_result_and_its_from_chain_only() {
    let src = "FROM alpine AS a\nRUN one\n\
               FROM a AS b\nRUN two\n\
               FROM scratch AS c\nCOPY --from=b /x /x\n";
    assert_eq!(layers_needed(&graph(src, &[])), [2]);
    assert_eq!(layers_needed(&graph(src, &["b"])), [0, 1]);
    assert_eq!(layers_needed(&graph(src, &["c", "b"])), [2]);
}
