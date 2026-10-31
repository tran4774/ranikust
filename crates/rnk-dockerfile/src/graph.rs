// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/dockerfile2llb/convert.go, convert_runmount.go and
// validations.go at tag v0.34.0 (buildDispatchStates, resolveTarget,
// buildStageDependencyGraph, toCommand, detectRunMount,
// initOnBuildTriggers, validateCircularDependency, allReachableStages).
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: an explicit graph of stage indices instead of LLB
// states; a list of targets, of which the CLI passes at most one
// (ADR-0049).

//! The stage graph: which stage or image each stage builds on and reads
//! from, which stages the targets need, in an order that never depends on
//! hashing.

use std::collections::BTreeMap;

use crate::error::{Error, ErrorKind, Lines};
use crate::goutil::to_lower;
use crate::instructions::{Dockerfile, Instruction, Located, parse_instruction};
use crate::parser::parse_ast;
use crate::scope::{EnvList, Platform, StageScope, build_meta_args, default_args};
use crate::shell::{Lexer, NoEnv};

/// Inputs of the stage graph that come from the command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphOptions {
    /// `--build-arg` values.
    pub build_args: BTreeMap<String, String>,
    /// The platform the build runs on (`BUILDPLATFORM`…).
    pub build_platform: Platform,
    /// The platform the image is for (`TARGETPLATFORM`…).
    pub target_platform: Platform,
    /// `--target` values; empty means the last stage. Every listed stage is
    /// built; the first is the result. `ranikust` passes at most one
    /// (ADR-0049); the list type stays for callers that build several.
    pub targets: Vec<String>,
}

/// What a stage builds on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// `FROM scratch`: an empty root.
    Scratch,
    /// An image reference, with build arguments expanded; not normalised.
    Image(String),
    /// An earlier stage, by index.
    Stage(usize),
}

/// What a `COPY --from` or a `RUN --mount from=` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A stage, by index.
    Stage(usize),
    /// An image reference.
    Image(String),
}

/// Where in a stage a reference is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The stage's instruction at this index.
    Instruction(usize),
    /// The `ONBUILD` trigger at this index (triggers run first).
    Trigger(usize),
}

/// How a stage reads from another stage or image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    /// `COPY --from`.
    CopyFrom,
    /// `RUN --mount from=`; the index of the mount in the instruction.
    MountFrom(usize),
}

/// A `COPY --from` or `RUN --mount from=` of a stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The instruction it is in.
    pub origin: Origin,
    /// `COPY --from` or a mount.
    pub kind: ReferenceKind,
    /// The value as written.
    pub written: String,
    /// What it resolves to.
    pub source: Source,
    /// Lines of the instruction.
    pub lines: Lines,
}

/// One stage of the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageNode {
    /// The stage's name, lower-cased.
    pub name: Option<String>,
    /// The name, or `stage-N` for an unnamed stage, as BuildKit shows it.
    pub display_name: String,
    /// What the stage builds on, after build-argument expansion.
    pub base: Base,
    /// `--platform`, expanded; not validated.
    pub platform: Option<String>,
    /// Lines of the `FROM`.
    pub lines: Lines,
    /// `ONBUILD` triggers of a base stage, parsed; they run before the
    /// stage's own instructions. Filled for stages the targets need.
    // Triggers of a base image come from its config, which the caller
    // pulls; a separate step extends the graph with them until no new
    // dependency appears (ADR-0032, as BuildKit's `initOnBuildTriggers`).
    pub triggers: Vec<Located<Instruction>>,
    /// `COPY --from` and `RUN --mount from=` references, triggers first,
    /// then in instruction order.
    pub references: Vec<Reference>,
}

impl StageNode {
    /// The stages this one needs, base first, without duplicates.
    #[must_use]
    pub fn dependencies(&self) -> Vec<usize> {
        let mut out = Vec::new();
        if let Base::Stage(b) = self.base {
            out.push(b);
        }
        for r in &self.references {
            if let Source::Stage(s) = r.source
                && !out.contains(&s)
            {
                out.push(s);
            }
        }
        out
    }

    /// Images the stage reads besides its base, in order, without
    /// duplicates.
    #[must_use]
    pub fn images(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for r in &self.references {
            if let Source::Image(i) = &r.source
                && !out.contains(&i.as_str())
            {
                out.push(i);
            }
        }
        out
    }
}

/// The stages of a Dockerfile, their dependencies and the targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageGraph {
    /// The lexer for the Dockerfile's escape character.
    pub lexer: Lexer,
    /// Built-in and global `ARG`s: visible to `FROM`, and to a stage's
    /// `ARG` without a default.
    pub global_args: EnvList,
    /// `--build-arg` values.
    pub build_args: BTreeMap<String, String>,
    /// Every stage, in file order.
    pub stages: Vec<StageNode>,
    /// The target stages; the first is the result.
    pub targets: Vec<usize>,
    /// The stages the targets need, targets included, in file order.
    pub needed: Vec<usize>,
}

impl StageGraph {
    /// The stage whose image is the result.
    #[must_use]
    pub fn primary_target(&self) -> usize {
        self.targets.first().copied().unwrap_or_default()
    }

    /// The variable scope at the start of a stage built on an image whose
    /// config has `image_env`. For `FROM <stage>`, clone the base stage's
    /// final scope instead.
    #[must_use]
    pub fn scope_from_image<S: AsRef<str>>(&self, image_env: &[S]) -> StageScope {
        StageScope::from_image(self.lexer, &self.global_args, &self.build_args, image_env)
    }
}

fn graph_err(lines: Lines, message: impl Into<String>) -> Error {
    Error::at(ErrorKind::Graph, lines, message)
}

/// Builds the stage graph: expands `FROM` with the global `ARG`s, resolves
/// stage references (`FROM <stage>`, `COPY --from`, `RUN --mount from=`)
/// and `ONBUILD` triggers of local base stages, checks for cycles and
/// unknown targets, and keeps the stages the targets need.
///
/// Resolution follows BuildKit: `FROM` matches only earlier stage names,
/// exactly; `COPY --from` takes a stage index or a stage name in any case,
/// anything else is an image; `RUN --mount from=` takes a name (no index);
/// a stage may only read from stages defined before it.
///
/// # Errors
///
/// [`ErrorKind::Expansion`] when a global `ARG`, a base name or a platform
/// cannot be expanded; [`ErrorKind::Graph`] for an empty base name, a
/// variable in `COPY --from`, a bad stage index, a cycle, a reference to a
/// later stage or an unknown target (the only error without lines).
pub fn stage_graph(df: &Dockerfile, opts: &GraphOptions) -> Result<StageGraph, Error> {
    let Some(last) = df.stages.last() else {
        let lines = df.meta_args.last().map(|a| a.lines);
        return Err(Error::new(
            ErrorKind::Graph,
            lines,
            "dockerfile contains no stages to build",
        ));
    };
    let lexer = Lexer::new(df.escape);
    let target_stage = match opts.targets.first() {
        Some(t) => t.clone(),
        None => last.name.clone().unwrap_or_default(),
    };
    let defaults = default_args(
        &opts.build_platform,
        &opts.target_platform,
        &opts.build_args,
        &target_stage,
    );
    let global_args = build_meta_args(defaults, &lexer, &df.meta_args, &opts.build_args)?;

    // Stages and their bases. `FROM` sees only the names defined before it;
    // a later stage of the same name replaces an earlier one.
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut stages = Vec::with_capacity(df.stages.len());
    for (i, st) in df.stages.iter().enumerate() {
        let base = lexer
            .process_word(&st.base, &global_args)
            .map_err(|e| Error::at(ErrorKind::Expansion, st.lines, e.to_string()))?;
        if base.is_empty() {
            return Err(graph_err(
                st.lines,
                format!("base name ({}) should not be blank", st.base),
            ));
        }
        let platform = match &st.platform {
            None => None,
            Some(p) => {
                let v = lexer.process_word(p, &global_args).map_err(|e| {
                    Error::at(
                        ErrorKind::Expansion,
                        st.lines,
                        format!("failed to process arguments for platform {p}: {e}"),
                    )
                })?;
                if v.is_empty() {
                    return Err(graph_err(
                        st.lines,
                        format!("empty platform value from expression {p}"),
                    ));
                }
                Some(v)
            }
        };
        // BuildKit matches the base exactly (names are lower case); kaniko
        // matches case-insensitively. ranikust follows BuildKit (Q-051).
        let base = match names.get(&base) {
            Some(&j) => Base::Stage(j),
            None if base == "scratch" => Base::Scratch,
            None => Base::Image(base),
        };
        if let Some(n) = &st.name {
            names.insert(n.clone(), i);
        }
        stages.push(StageNode {
            name: st.name.clone(),
            display_name: st.name.clone().unwrap_or_else(|| format!("stage-{i}")),
            base,
            platform,
            lines: st.lines,
            triggers: Vec::new(),
            references: Vec::new(),
        });
    }

    // References of the stages' own instructions.
    let n_stages = stages.len();
    for (i, st) in df.stages.iter().enumerate() {
        let mut refs = Vec::new();
        for (k, ins) in st.instructions.iter().enumerate() {
            references(
                ins,
                Origin::Instruction(k),
                &names,
                n_stages,
                &lexer,
                &mut refs,
            )?;
        }
        stages[i].references = refs;
    }
    check_cycles(&stages)?;

    // Every listed target is built; the first is the result.
    let mut targets = Vec::new();
    if opts.targets.is_empty() {
        targets.push(n_stages - 1);
    }
    for t in &opts.targets {
        let Some(&i) = names.get(&to_lower(t)) else {
            return Err(Error::new(
                ErrorKind::Graph,
                None,
                format!("target stage \"{t}\" could not be found"),
            ));
        };
        if !targets.contains(&i) {
            targets.push(i);
        }
    }

    // ONBUILD triggers of local base stages can add references, so resolve
    // them for the needed stages until nothing changes.
    let mut with_triggers = vec![false; n_stages];
    let needed = loop {
        let needed = reachable(&stages, &targets);
        let mut changed = false;
        for &i in &needed {
            if with_triggers[i] {
                continue;
            }
            with_triggers[i] = true;
            if let Base::Stage(b) = stages[i].base {
                let triggers = parse_triggers(&df.stages[b].instructions, stages[i].lines)?;
                let mut refs = Vec::new();
                for (t, ins) in triggers.iter().enumerate() {
                    references(ins, Origin::Trigger(t), &names, n_stages, &lexer, &mut refs)?;
                }
                changed |= !refs.is_empty();
                refs.append(&mut stages[i].references);
                stages[i].references = refs;
                stages[i].triggers = triggers;
            }
        }
        if !changed {
            break needed;
        }
        check_cycles(&stages)?;
    };

    // Stages are built in file order, so a stage can read only from stages
    // defined before it.
    for &i in &needed {
        for r in &stages[i].references {
            if let Source::Stage(j) = r.source
                && j > i
            {
                let message = match r.kind {
                    ReferenceKind::CopyFrom => format!(
                        "cannot copy from stage \"{}\", it needs to be defined before current stage \"{}\"",
                        r.written, stages[i].display_name
                    ),
                    ReferenceKind::MountFrom(_) => format!(
                        "cannot mount from stage \"{}\", stage needs to be defined before current command",
                        r.written
                    ),
                };
                return Err(graph_err(r.lines, message));
            }
        }
    }

    Ok(StageGraph {
        lexer,
        global_args,
        build_args: opts.build_args.clone(),
        stages,
        targets,
        needed,
    })
}

/// BuildKit's `toCommand` and `detectRunMount`: the stage or image a
/// `COPY --from` or `RUN --mount from=` reads.
fn references(
    ins: &Located<Instruction>,
    origin: Origin,
    names: &BTreeMap<String, usize>,
    n_stages: usize,
    lexer: &Lexer,
    out: &mut Vec<Reference>,
) -> Result<(), Error> {
    match &ins.value {
        Instruction::Copy(c) if !c.from.is_empty() => {
            let unchanged = lexer
                .process_word(&c.from, &NoEnv)
                .is_ok_and(|v| v == c.from);
            if !unchanged {
                return Err(graph_err(
                    ins.lines,
                    "variable expansion is not supported for --from, define a new stage with FROM using ARG from global scope as a workaround",
                ));
            }
            let source = match c.from.parse::<i64>() {
                // BuildKit also counts images referenced earlier in the file
                // as indices past the last stage; that accident is not kept.
                Ok(n) => match usize::try_from(n).ok().filter(|&n| n < n_stages) {
                    Some(n) => Source::Stage(n),
                    None => return Err(graph_err(ins.lines, format!("invalid stage index {n}"))),
                },
                Err(_) => match names.get(&to_lower(&c.from)) {
                    Some(&j) => Source::Stage(j),
                    None => Source::Image(c.from.clone()),
                },
            };
            out.push(Reference {
                origin,
                kind: ReferenceKind::CopyFrom,
                written: c.from.clone(),
                source,
                lines: ins.lines,
            });
        }
        Instruction::Run(r) => {
            for (m, mount) in r.mounts.iter().enumerate() {
                if mount.from.is_empty() {
                    continue;
                }
                let source = match names.get(&to_lower(&mount.from)) {
                    Some(&j) => Source::Stage(j),
                    None => Source::Image(mount.from.clone()),
                };
                out.push(Reference {
                    origin,
                    kind: ReferenceKind::MountFrom(m),
                    written: mount.from.clone(),
                    source,
                    lines: ins.lines,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

/// BuildKit's `initOnBuildTriggers` for a local base stage: each `ONBUILD`
/// expression is parsed on its own (with the default escape character) and
/// placed at the `FROM` of the stage that runs it.
fn parse_triggers(
    base_instructions: &[Located<Instruction>],
    at: Lines,
) -> Result<Vec<Located<Instruction>>, Error> {
    let mut out = Vec::new();
    for ins in base_instructions {
        let Instruction::Onbuild(expr) = &ins.value else {
            continue;
        };
        let reloc = |e: Error| Error::new(e.kind(), Some(at), e.message());
        let ast = parse_ast(expr).map_err(reloc)?;
        let [node] = ast.nodes.as_slice() else {
            return Err(graph_err(
                at,
                "onbuild trigger should be a single expression",
            ));
        };
        let mut node = node.clone();
        node.lines = at;
        out.push(parse_instruction(&node).map_err(reloc)?);
    }
    Ok(out)
}

/// The stages the targets need: their bases and the stages they read,
/// transitively, in file order.
fn reachable(stages: &[StageNode], targets: &[usize]) -> Vec<usize> {
    let mut seen = vec![false; stages.len()];
    let mut stack: Vec<usize> = targets.to_vec();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        stack.extend(stages[i].dependencies());
    }
    (0..stages.len()).filter(|&i| seen[i]).collect()
}

/// BuildKit's `validateCircularDependency`, over `COPY --from` and
/// `RUN --mount from=` references, deterministic.
fn check_cycles(stages: &[StageNode]) -> Result<(), Error> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let mut marks = vec![Mark::New; stages.len()];
    for start in 0..stages.len() {
        if marks[start] != Mark::New {
            continue;
        }
        // Iterative depth-first search: (stage, next reference to visit).
        let mut stack = vec![(start, 0usize)];
        marks[start] = Mark::Open;
        while let Some(&mut (s, ref mut next)) = stack.last_mut() {
            let refs = &stages[s].references;
            let Some(r) = refs.get(*next) else {
                marks[s] = Mark::Done;
                stack.pop();
                continue;
            };
            *next += 1;
            let Source::Stage(d) = r.source else {
                continue;
            };
            match marks[d] {
                Mark::Open => {
                    return Err(graph_err(
                        r.lines,
                        format!(
                            "circular dependency detected on stage: {}",
                            stages[start].display_name
                        ),
                    ));
                }
                Mark::New => {
                    marks[d] = Mark::Open;
                    stack.push((d, 0));
                }
                Mark::Done => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
