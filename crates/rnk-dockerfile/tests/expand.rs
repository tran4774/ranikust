//! Expansion goldens: `tests/golden/expand/` holds what BuildKit's
//! `dockerfile2llb` makes of kaniko's 200 integration Dockerfiles, with the
//! build arguments kaniko's tests pass and a fake base image (see
//! `golden-gen/expand.go`): the target image's `Env`, labels, user, volumes
//! and stop signal, and the arguments and environment of every `RUN`.
//!
//! This test walks the same stages with the stage graph and `StageScope` and
//! compares. It checks global and stage `ARG`s, `--build-arg`, built-in
//! arguments, `ENV` order, `FROM <stage>` inheritance, `ONBUILD` triggers of
//! local stages and the `PATH` default.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rnk_dockerfile::{
    Base, GraphOptions, Instruction, Platform, Run, StageScope, chomp_heredoc_content, parse,
    parse_heredoc_marker, stage_graph,
};
use serde_json::{Value, json};

/// The config of every image the generator's fake resolver returns.
const BASE_ENV: [&str; 2] = ["PATH=/usr/local/bin:/usr/bin:/bin", "BASE_VAR=from-base"];
const BASE_LABEL: (&str, &str) = ("base.label", "base");

/// Files BuildKit rejects only when it converts the instructions to LLB, for
/// reasons outside the parser: the builder must check these.
const DISPATCH_ERRORS: &[(&str, &str)] = &[
    (
        "Dockerfile_test_issue_3373_2",
        "ADD --checksum with a local source (rnk-builder)",
    ),
    (
        "Dockerfile_test_issue_3373_4",
        "ADD --checksum with several sources (rnk-builder)",
    ),
    (
        "Dockerfile_test_issue_519",
        "FROM BUG_stage: BuildKit matches stage names exactly and rejects the image name; kaniko matches case-insensitively (open question Q-050)",
    ),
];

/// The image state a stage carries to the next instruction and to stages
/// built on it.
#[derive(Clone)]
struct State {
    scope: StageScope,
    labels: BTreeMap<String, String>,
    user: String,
    volumes: BTreeSet<String>,
    stop_signal: String,
    shell: Vec<String>,
}

/// BuildKit's `dispatchRun` arguments: heredoc handling, then the shell.
fn run_args(r: &Run, shell: &[String]) -> Vec<String> {
    let mut args = r.cmd.args.clone();
    if let (Some(first), [file, ..]) = (args.first().cloned(), r.cmd.files.as_slice()) {
        if args.len() == 1 && parse_heredoc_marker(&first).is_some() {
            let data = if file.chomp {
                chomp_heredoc_content(&file.data)
            } else {
                file.data.clone()
            };
            args = if data.starts_with("#!") {
                vec![format!("/dev/pipes/{}", file.name)]
            } else {
                vec![data]
            };
        } else {
            let mut s = first;
            for f in &r.cmd.files {
                s.push('\n');
                s.push_str(&f.data);
                s.push_str(&f.name);
            }
            args = vec![s];
        }
    }
    if r.cmd.prepend_shell {
        let mut full = if shell.is_empty() {
            vec!["/bin/sh".to_owned(), "-c".to_owned()]
        } else {
            shell.to_vec()
        };
        full.extend(args);
        full
    } else {
        args
    }
}

fn walk(src: &str, build_args: BTreeMap<String, String>) -> Result<Value, String> {
    let df = parse(src).map_err(|e| e.to_string())?;
    let opts = GraphOptions {
        build_args,
        build_platform: Platform::new("linux", "amd64"),
        target_platform: Platform::new("linux", "amd64"),
        targets: Vec::new(),
    };
    let g = stage_graph(&df, &opts).map_err(|e| e.to_string())?;
    let mut finals: BTreeMap<usize, State> = BTreeMap::new();
    let mut runs: Vec<Value> = Vec::new();
    for &i in &g.needed {
        let mut st = match &g.stages[i].base {
            Base::Stage(b) => finals.get(b).cloned().ok_or("base not built")?,
            Base::Image(_) => State {
                scope: g.scope_from_image(&BASE_ENV),
                labels: [BASE_LABEL]
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                user: String::new(),
                volumes: BTreeSet::new(),
                stop_signal: String::new(),
                shell: Vec::new(),
            },
            Base::Scratch => State {
                scope: g.scope_from_image::<&str>(&[]),
                labels: BTreeMap::new(),
                user: String::new(),
                volumes: BTreeSet::new(),
                stop_signal: String::new(),
                shell: Vec::new(),
            },
        };
        for ins in g.stages[i]
            .triggers
            .iter()
            .chain(&df.stages[i].instructions)
        {
            let e = st.scope.apply(ins).map_err(|e| e.to_string())?;
            match e.value {
                Instruction::Label(kvs) => {
                    for kv in kvs {
                        st.labels.insert(kv.key, kv.value);
                    }
                }
                Instruction::User(u) => st.user = u,
                Instruction::Volume(v) => st.volumes.extend(v),
                Instruction::StopSignal(s) => st.stop_signal = s,
                Instruction::Shell(s) => st.shell = s,
                Instruction::Run(r) => {
                    let mut env = st.scope.env().to_entries();
                    env.sort();
                    runs.push(json!({"args": run_args(&r, &st.shell), "env": env}));
                }
                _ => {}
            }
        }
        finals.insert(i, st);
    }
    let t = finals.get(&g.primary_target()).ok_or("target not built")?;
    Ok(json!({
        "env": t.scope.config_env().to_entries(),
        "labels": t.labels,
        "user": t.user,
        "volumes": t.volumes,
        "stop_signal": t.stop_signal,
        "runs": sorted(runs),
    }))
}

/// Sorted and without duplicates: LLB is content-addressed, so BuildKit's
/// definition holds one op for identical `RUN`s on identical inputs.
fn sorted(mut runs: Vec<Value>) -> Vec<Value> {
    runs.sort_by_key(Value::to_string);
    runs.dedup();
    runs
}

#[test]
fn kaniko_expansion_matches_buildkit() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut goldens: Vec<PathBuf> = fs::read_dir(tests.join("golden/expand"))
        .expect("goldens")
        .map(|e| e.expect("entry").path())
        .collect();
    goldens.sort();
    assert_eq!(goldens.len(), 200);
    let (mut matched, mut skipped, mut failed) = (0, Vec::new(), Vec::new());
    for path in goldens {
        let golden: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("json");
        let file = golden["file"].as_str().expect("file");
        let src = fs::read_to_string(tests.join("fixtures/kaniko").join(file)).expect("fixture");
        let build_args: BTreeMap<String, String> =
            serde_json::from_value(golden["build_args"].clone()).expect("build args");
        let got = walk(&src, build_args);
        if golden["ok"] != true {
            match DISPATCH_ERRORS.iter().find(|(f, _)| *f == file) {
                Some((_, why)) => skipped.push(format!("{file}: {why}")),
                None => failed.push(format!(
                    "{file}: BuildKit rejects it ({}) and the reason is not listed",
                    golden["error"]
                )),
            }
            continue;
        }
        let got = match got {
            Ok(v) => v,
            Err(e) => {
                failed.push(format!("{file}: {e}"));
                continue;
            }
        };
        let mut want = json!({});
        for k in ["env", "labels", "user", "volumes", "stop_signal"] {
            want[k] = golden[k].clone();
        }
        want["runs"] = json!(sorted(golden["runs"].as_array().expect("runs").clone()));
        if got == want {
            matched += 1;
        } else {
            let field = ["env", "labels", "user", "volumes", "stop_signal", "runs"]
                .into_iter()
                .find(|k| got[k] != want[k])
                .unwrap_or("?");
            failed.push(format!(
                "{file}: {field} differs\n  got  {}\n  want {}",
                got[field], want[field]
            ));
        }
    }
    eprintln!(
        "expansion: {matched} of 200 files match BuildKit; {} rejected by BuildKit's dispatcher:\n  {}",
        skipped.len(),
        skipped.join("\n  ")
    );
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert_eq!(matched + skipped.len(), 200);
}
