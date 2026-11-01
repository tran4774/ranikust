use super::*;
use crate::instructions::parse;

fn opts(targets: &[&str], build_args: &[(&str, &str)]) -> GraphOptions {
    GraphOptions {
        build_args: build_args
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        build_platform: Platform::new("linux", "amd64"),
        target_platform: Platform::new("linux", "arm64"),
        targets: targets.iter().map(|t| (*t).to_owned()).collect(),
    }
}

fn graph(src: &str, o: &GraphOptions) -> Result<StageGraph, Error> {
    stage_graph(&parse(src).unwrap_or_else(|e| panic!("{e}")), o)
}

fn ok(src: &str) -> StageGraph {
    graph(src, &opts(&[], &[])).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn err(src: &str) -> Error {
    graph(src, &opts(&[], &[])).expect_err(src)
}

/// ADR-0052, D2: a bare word naming no stage is refused instead of pulled
/// as an image (moby/buildkit#7091); with a tag, digest, registry or path
/// it is an image.
#[test]
fn copy_from_a_bare_word_that_names_no_stage_is_refused() {
    for name in ["buildr", "busybox", "scratch"] {
        let e = err(&format!(
            "FROM alpine AS builder\nCOPY --from={name} /a /a\n"
        ));
        assert!(
            e.to_string()
                .contains(&format!("no stage is named \"{name}\"")),
            "{e}"
        );
        assert!(e.to_string().contains(&format!("{name}:latest")), "{e}");
    }
    let g = ok("FROM alpine\n\
         COPY --from=busybox:1 /a /a\n\
         COPY --from=busybox@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa /b /b\n\
         COPY --from=library/busybox /c /c\n\
         COPY --from=localhost:5000/x /d /d\n");
    assert_eq!(g.stages[0].references.len(), 4);
    assert!(
        g.stages[0]
            .references
            .iter()
            .all(|r| matches!(r.source, Source::Image(_)))
    );
}

#[test]
fn bases_and_references() {
    let g = ok("FROM alpine AS build\n\
         RUN make\n\
         FROM build AS test\n\
         FROM scratch\n\
         COPY --from=build /a /a\n\
         COPY --from=0 /b /b\n\
         COPY --from=BUILD /c /c\n\
         COPY --from=busybox:1 /d /d\n\
         RUN --mount=type=cache,from=test,target=/t --mount=from=0,target=/z true\n");
    assert_eq!(g.stages[0].base, Base::Image("alpine".to_owned()));
    assert_eq!(g.stages[1].base, Base::Stage(0));
    assert_eq!(g.stages[2].base, Base::Scratch);
    assert_eq!(g.stages[2].display_name, "stage-2");
    let srcs: Vec<&Source> = g.stages[2].references.iter().map(|r| &r.source).collect();
    assert_eq!(
        srcs,
        [
            &Source::Stage(0),
            &Source::Stage(0),
            &Source::Stage(0),
            &Source::Image("busybox:1".to_owned()),
            &Source::Stage(1),
            // Mounts take names only: `0` is an image.
            &Source::Image("0".to_owned()),
        ]
    );
    assert_eq!(g.stages[2].references[4].kind, ReferenceKind::MountFrom(0));
    assert_eq!(g.stages[2].references[4].origin, Origin::Instruction(4));
    assert_eq!(g.stages[2].dependencies(), [0, 1]);
    assert_eq!(g.stages[2].images(), ["busybox:1", "0"]);
    assert_eq!(g.targets, [2]);
    assert_eq!(g.needed, [0, 1, 2]);
}

#[test]
fn from_matches_only_earlier_names_exactly() {
    let g = ok("FROM later AS first\nFROM first AS later\nFROM First\n");
    assert_eq!(g.stages[0].base, Base::Image("later".to_owned()));
    assert_eq!(g.stages[1].base, Base::Stage(0));
    assert_eq!(g.stages[2].base, Base::Image("First".to_owned()));
    // A later stage of the same name wins; a stage named scratch shadows
    // the empty image.
    let g = ok("FROM a AS x\nFROM b AS x\nFROM x AS scratch\nFROM scratch\n");
    assert_eq!(g.stages[2].base, Base::Stage(1));
    assert_eq!(g.stages[3].base, Base::Stage(2));
}

#[test]
fn global_args_and_builtins() {
    let src = "ARG REPO=library\nARG TAG\nARG IMAGE=$REPO/alpine:${TAG:-latest}\n\
               FROM --platform=$BUILDPLATFORM $IMAGE AS build\n\
               FROM --platform=${TARGETPLATFORM} build\n";
    let g = graph(src, &opts(&[], &[])).unwrap();
    assert_eq!(
        g.stages[0].base,
        Base::Image("library/alpine:latest".to_owned())
    );
    assert_eq!(g.stages[0].platform.as_deref(), Some("linux/amd64"));
    assert_eq!(g.stages[1].platform.as_deref(), Some("linux/arm64"));
    assert_eq!(g.global_args.get("TARGETSTAGE"), Some("default"));
    assert_eq!(g.global_args.get("TAG"), None);

    let g = graph(src, &opts(&["BUILD"], &[("TAG", "3.20"), ("REPO", "me")])).unwrap();
    assert_eq!(g.stages[0].base, Base::Image("me/alpine:3.20".to_owned()));
    assert_eq!(g.global_args.get("TARGETSTAGE"), Some("BUILD"));
    // A build argument for an undeclared name is not a global ARG.
    let g = graph("FROM a$X\n", &opts(&[], &[("X", "1")])).unwrap();
    assert_eq!(g.stages[0].base, Base::Image("a".to_owned()));
}

#[test]
fn base_and_platform_errors() {
    let e = err("ARG B\nFROM $B\n");
    assert_eq!(e.message(), "base name ($B) should not be blank");
    assert_eq!(e.lines(), Some(Lines::single(2)));
    let e = err("FROM --platform=$NONE a\n");
    assert_eq!(e.message(), "empty platform value from expression $NONE");
    let e = err("ARG A=${X:?unset}\nFROM a\n");
    assert_eq!(e.kind(), ErrorKind::Expansion);
    assert_eq!(e.lines(), Some(Lines::single(1)));
    let e = err("FROM a${X\n");
    assert_eq!(e.kind(), ErrorKind::Expansion);
}

#[test]
fn copy_from_errors() {
    for (src, msg) in [
        (
            "FROM a\nCOPY --from=$X a b\n",
            "variable expansion is not supported for --from",
        ),
        (
            "FROM a\nCOPY --from=a\\\"b a b\n",
            "variable expansion is not supported for --from",
        ),
        ("FROM a\nCOPY --from=1 a b\n", "invalid stage index 1"),
        ("FROM a\nCOPY --from=-1 a b\n", "invalid stage index -1"),
    ] {
        let e = err(src);
        assert!(e.message().starts_with(msg), "{src}: {e}");
        assert_eq!(e.lines(), Some(Lines::single(2)));
        assert_eq!(e.kind(), ErrorKind::Graph);
    }
}

#[test]
fn cycles() {
    let e = err("FROM a AS x\nCOPY --from=y a b\nFROM a AS y\nCOPY --from=x a b\n");
    assert_eq!(e.message(), "circular dependency detected on stage: x");
    assert!(e.lines().is_some());
    let e = err("FROM a AS self\nRUN --mount=from=self,target=/x true\n");
    assert_eq!(e.message(), "circular dependency detected on stage: self");
    assert_eq!(e.lines(), Some(Lines::single(2)));
}

#[test]
fn forward_references() {
    let src = "FROM a AS first\nCOPY --from=second a b\nFROM a AS second\n";
    let e = graph(src, &opts(&["first"], &[])).unwrap_err();
    assert_eq!(
        e.message(),
        "cannot copy from stage \"second\", it needs to be defined before current stage \"first\""
    );
    assert_eq!(e.lines(), Some(Lines::single(2)));
    // Only for the stages the targets need: by default the last one.
    let g = graph(src, &opts(&[], &[])).unwrap();
    assert_eq!(g.needed, [1]);
    let e = graph(
        "FROM a AS first\nRUN --mount=from=second,target=/x y\nFROM a AS second\n",
        &opts(&["first"], &[]),
    )
    .unwrap_err();
    assert!(
        e.message()
            .starts_with("cannot mount from stage \"second\"")
    );
}

#[test]
fn targets() {
    let src = "FROM a AS one\nFROM b AS two\nCOPY --from=one a b\nFROM c AS three\n";
    let g = graph(src, &opts(&[], &[])).unwrap();
    assert_eq!((g.targets.clone(), g.needed.clone()), (vec![2], vec![2]));
    let g = graph(src, &opts(&["TWO"], &[])).unwrap();
    assert_eq!((g.targets.clone(), g.needed.clone()), (vec![1], vec![0, 1]));
    assert_eq!(g.primary_target(), 1);
    let g = graph(src, &opts(&["three", "two", "three"], &[])).unwrap();
    assert_eq!(
        (g.targets.clone(), g.needed.clone()),
        (vec![2, 1], vec![0, 1, 2])
    );
    let e = graph(src, &opts(&["four"], &[])).unwrap_err();
    assert_eq!(e.message(), "target stage \"four\" could not be found");
    assert_eq!(e.lines(), None);
}

#[test]
fn onbuild_triggers_of_local_bases() {
    let src = "FROM a AS dep\n\
               FROM a AS base\n\
               ONBUILD COPY --from=dep /x /y\n\
               ONBUILD RUN echo hi\n\
               FROM base AS child\n\
               RUN true\n";
    let g = ok(src);
    let child = &g.stages[2];
    assert_eq!(child.triggers.len(), 2);
    assert_eq!(child.triggers[0].lines, child.lines);
    assert_eq!(child.references[0].origin, Origin::Trigger(0));
    assert_eq!(child.references[0].source, Source::Stage(0));
    assert_eq!(g.needed, [0, 1, 2]);
    // Triggers of a base stage are not run by the base itself.
    assert!(g.stages[1].triggers.is_empty());

    let e = graph(
        "FROM a AS base\nONBUILD COPY --from=later /x /y\nFROM base AS child\nFROM a AS later\n",
        &opts(&["child"], &[]),
    )
    .unwrap_err();
    assert!(
        e.message().starts_with("cannot copy from stage \"later\""),
        "{e}"
    );
}

#[test]
fn no_stages() {
    let df = parse("ARG A\n").unwrap();
    let e = stage_graph(&df, &opts(&[], &[])).unwrap_err();
    assert_eq!(e.message(), "dockerfile contains no stages to build");
    assert_eq!(e.lines(), Some(Lines::single(1)));
}

#[test]
fn scopes_follow_stages() {
    let src = "ARG V=1\nFROM a AS base\nARG V\nENV E=$V\nFROM base\nRUN echo\n";
    let df = parse(src).unwrap();
    let g = stage_graph(&df, &opts(&[], &[])).unwrap();
    let mut base = g.scope_from_image(&["PATH=/bin", "HOME=/root"]);
    for i in &df.stages[0].instructions {
        base.apply(i).unwrap();
    }
    assert_eq!(
        base.config_env().to_entries(),
        ["PATH=/bin", "HOME=/root", "E=1"]
    );
    // FROM <stage>: the child starts from the base stage's final scope, ARG
    // values included, as BuildKit's LLB state carries them.
    let child = base.clone();
    assert_eq!(child.env().get("V"), Some("1"));
}
