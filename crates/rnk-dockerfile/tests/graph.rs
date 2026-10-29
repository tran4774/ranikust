//! The stage graph and the stage scopes over kaniko's 200 integration
//! Dockerfiles: every file gets a graph, and every instruction of every
//! needed stage expands, starting from an image that sets only `PATH`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use rnk_dockerfile::{
    Base, GraphOptions, Instruction, Platform, Source, StageScope, parse, stage_graph,
};

fn options() -> GraphOptions {
    GraphOptions {
        build_args: BTreeMap::new(),
        build_platform: Platform::new("linux", "amd64"),
        target_platform: Platform::new("linux", "amd64"),
        targets: Vec::new(),
    }
}

/// The build arguments kaniko's integration tests pass
/// (`kaniko@44aa7ddc0 integration/images.go:143-159`); `mz1007` gets its
/// base from the test itself (`integration_test.go:2017`).
fn options_for(file: &str) -> GraphOptions {
    let args: &[&str] = match file {
        "Dockerfile_test_run" | "Dockerfile_test_run_new" | "Dockerfile_test_run_redo" => {
            &["file=/file"]
        }
        "Dockerfile_test_workdir" => &["workdir=/arg/workdir"],
        "Dockerfile_test_add" => &["file=context/foo"],
        "Dockerfile_test_arg_secret" => &["SSH_PRIVATE_KEY", "SSH_PUBLIC_KEY=Pµbl1cK€Y"],
        "Dockerfile_test_onbuild" => &["file=/tmp/onbuild"],
        "Dockerfile_test_scratch" => &[
            "image=scratch",
            "hello=hello-value",
            "file=context/foo",
            "file3=context/b*",
        ],
        "Dockerfile_test_multistage" => &["file=/foo2"],
        "Dockerfile_test_issue_mz655" => &["BASE_TAG=1.37.0"],
        "Dockerfile_test_issue_3373" => {
            &["DIGEST=sha256:a680e90a30faae7b193edf63b088a11edf856f731d93c25f1fd7c1a21cf82d06"]
        }
        "Dockerfile_test_issue_mz1007" => &["BASE=127.0.0.2:5001/userb/test_issue_mz1007:base"],
        _ => &[],
    };
    let mut o = options();
    for a in args {
        let (k, v) = a.split_once('=').unwrap_or((a, ""));
        o.build_args.insert(k.to_owned(), v.to_owned());
    }
    o
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned()
}

/// Builds every needed stage's scope and expands its instructions.
fn walk(src: &str, opts: &GraphOptions) -> Result<(usize, usize), String> {
    let df = parse(src).map_err(|e| e.to_string())?;
    let g = stage_graph(&df, opts).map_err(|e| e.to_string())?;
    let mut finals: BTreeMap<usize, StageScope> = BTreeMap::new();
    let mut expanded = 0;
    for &i in &g.needed {
        let mut scope = match g.stages[i].base {
            Base::Stage(b) => finals
                .get(&b)
                .cloned()
                .ok_or_else(|| format!("base stage {b} of {i} not built first"))?,
            Base::Image(_) | Base::Scratch => g.scope_from_image(&["PATH=/usr/bin:/bin"]),
        };
        for ins in g.stages[i]
            .triggers
            .iter()
            .chain(&df.stages[i].instructions)
        {
            scope.apply(ins).map_err(|e| e.to_string())?;
            expanded += 1;
        }
        finals.insert(i, scope);
    }
    Ok((g.needed.len(), expanded))
}

#[test]
fn kaniko_dockerfiles_build_graphs() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kaniko");
    let mut names: Vec<_> = fs::read_dir(&dir)
        .expect("fixtures")
        .map(|e| e.expect("entry").path())
        .collect();
    names.sort();
    assert_eq!(names.len(), 200);
    let mut failures = Vec::new();
    let (mut stages, mut instructions) = (0, 0);
    for path in &names {
        let src = fs::read_to_string(path).expect("read");
        match walk(&src, &options_for(&file_name(path))) {
            Ok((s, i)) => {
                stages += s;
                instructions += i;
            }
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }
    eprintln!(
        "graphs: {} of 200 files, {stages} needed stages, {instructions} instructions expanded",
        200 - failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/kaniko")
        .join(name);
    fs::read_to_string(path).expect("fixture")
}

#[test]
fn kaniko_multistage_file() {
    let df = parse(&fixture("Dockerfile_test_multistage")).unwrap();
    let g = stage_graph(&df, &options_for("Dockerfile_test_multistage")).unwrap();
    // kaniko builds the same stages for this file (MakeKanikoStages keeps
    // the stages the last one needs).
    let last = g.stages.len() - 1;
    assert_eq!(g.targets, [last]);
    assert!(g.needed.contains(&last));
    for &i in &g.needed {
        for d in g.stages[i].dependencies() {
            assert!(d < i, "stage {i} depends on later stage {d}");
            assert!(g.needed.contains(&d));
        }
    }
}

#[test]
fn kaniko_args_in_from() {
    let src = fixture("Dockerfile_test_arg_multi_with_quotes");
    let df = parse(&src).unwrap();
    let g = stage_graph(&df, &options()).unwrap();
    assert!(matches!(&g.stages[0].base, Base::Image(i) if !i.contains('$')));
}

#[test]
fn copy_from_index_and_name_in_kaniko_files() {
    let mut seen = 0;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kaniko");
    for e in fs::read_dir(dir).expect("fixtures") {
        let path = e.expect("entry").path();
        let src = fs::read_to_string(&path).expect("read");
        let df = parse(&src).unwrap();
        let g = stage_graph(&df, &options_for(&file_name(&path))).unwrap();
        for (i, st) in df.stages.iter().enumerate() {
            for ins in &st.instructions {
                if let Instruction::Copy(c) = &ins.value {
                    if c.from.is_empty() {
                        continue;
                    }
                    seen += 1;
                    let r = g.stages[i]
                        .references
                        .iter()
                        .find(|r| r.lines == ins.lines)
                        .expect("reference recorded");
                    if let Source::Stage(j) = r.source {
                        assert!(j < g.stages.len());
                    }
                }
            }
        }
    }
    assert!(seen > 20, "only {seen} COPY --from in kaniko's files");
}
