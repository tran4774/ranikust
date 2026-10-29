//! Golden tests against BuildKit v0.34.0.
//!
//! `tests/golden/` holds BuildKit's output for the Dockerfiles in
//! `tests/fixtures/` and for generated shell-lexer cases, written by the Go
//! program in `tests/golden-gen/` (see its README). These tests compute the
//! same JSON from rnk-dockerfile and compare.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rnk_dockerfile::{
    Ast, CmdLine, Dockerfile, Instruction, Lexer, Located, SourceContent, from_ast, parse_ast,
};
use serde_json::{Value, json};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The raw syntax tree in the golden format.
fn ast_json(ast: &Ast) -> Value {
    Value::Array(
        ast.nodes
            .iter()
            .map(|n| {
                json!({
                    "dump": n.dump(),
                    "lines": [n.lines.start, n.lines.end],
                    "heredocs": n.heredocs.iter().map(|h| json!({
                        "name": h.name,
                        "fd": h.file_descriptor,
                        "expand": h.expand,
                        "chomp": h.chomp,
                        "content": h.content,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// BuildKit reports the line of a parser-directive error one too low (it
/// counts from 0 there) and line 0 for an empty file; rnk-dockerfile reports
/// the real line.
fn error_line_matches(golden: &Value, got: Option<usize>) -> bool {
    let want = golden["error_line"].as_u64().unwrap_or(0);
    let message = golden["error"].as_str().unwrap_or("");
    let got = got.unwrap_or(0) as u64;
    if want == 0 {
        return true;
    }
    if message.contains("parser directive") || message.contains("escape token") {
        return got == want + 1;
    }
    got == want
}

fn lines(l: rnk_dockerfile::Lines) -> Value {
    json!([l.start, l.end])
}

fn shell_dependent(m: &mut serde_json::Map<String, Value>, c: &CmdLine) {
    m.insert("prepend_shell".into(), json!(c.prepend_shell));
    m.insert("cmdline".into(), json!(c.args));
    let files: Vec<Value> = c
        .files
        .iter()
        .map(|f| json!([f.name, f.data, f.chomp]))
        .collect();
    m.insert("files".into(), json!(files));
}

fn contents(c: &[SourceContent]) -> Value {
    json!(
        c.iter()
            .map(|c| json!([c.path, c.data, c.expand]))
            .collect::<Vec<_>>()
    )
}

fn mounts_json(m: &mut serde_json::Map<String, Value>, r: &rnk_dockerfile::Run) {
    // As the generator does: parse the mounts with an expander that changes
    // nothing.
    let mut r = r.clone();
    if r.parse_mounts(&mut |w: &str| Ok(w.to_owned())).is_err() {
        m.insert("mount_error".into(), json!(true));
        return;
    }
    let mounts: Vec<Value> = r
        .mounts
        .iter()
        .map(|mt| {
            json!({
                "type": mt.kind.as_str(),
                "from": mt.from,
                "source": mt.source,
                "target": mt.target,
                "readonly": mt.read_only,
                "size": mt.size_limit,
                "id": mt.cache_id,
                "sharing": mt.sharing.map_or("", |s| s.as_str()),
                "required": mt.required,
                "env": mt.env,
                "mode": mt.mode,
                "uid": mt.uid,
                "gid": mt.gid,
            })
        })
        .collect();
    m.insert("mounts".into(), json!(mounts));
}

/// One instruction in the golden format of `golden-gen/main.go`.
fn command_json(i: &Located<Instruction>) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("cmd".into(), json!(i.keyword.to_lowercase()));
    m.insert("lines".into(), lines(i.lines));
    match &i.value {
        Instruction::Env(kv) | Instruction::Label(kv) => {
            let pairs: Vec<Value> = kv
                .iter()
                .map(|p| json!([p.key, p.value, p.no_delim]))
                .collect();
            m.insert("pairs".into(), json!(pairs));
        }
        Instruction::Arg(args) => {
            let args: Vec<Value> = args.iter().map(|a| json!([a.key, a.value])).collect();
            m.insert("args".into(), json!(args));
        }
        Instruction::Maintainer(v)
        | Instruction::Workdir(v)
        | Instruction::User(v)
        | Instruction::StopSignal(v) => {
            m.insert("value".into(), json!(v));
        }
        Instruction::Expose(v) | Instruction::Volume(v) | Instruction::Shell(v) => {
            m.insert("values".into(), json!(v));
        }
        Instruction::Onbuild(e) => {
            m.insert("expression".into(), json!(e));
        }
        Instruction::Cmd(c) | Instruction::Entrypoint(c) => shell_dependent(&mut m, c),
        Instruction::Run(r) => {
            shell_dependent(&mut m, &r.cmd);
            m.insert("flags_used".into(), json!(r.flags_used));
            m.insert("network".into(), json!(r.network.as_str()));
            m.insert("security".into(), json!(r.security.as_str()));
            let devices: Vec<Value> = r
                .devices
                .iter()
                .map(|d| json!([d.name, d.required]))
                .collect();
            m.insert("devices".into(), json!(devices));
            mounts_json(&mut m, r);
        }
        Instruction::Copy(c) => {
            m.insert("sources".into(), json!(c.sources));
            m.insert("dest".into(), json!(c.dest));
            m.insert("contents".into(), contents(&c.contents));
            m.insert("from".into(), json!(c.from));
            m.insert("chown".into(), json!(c.chown));
            m.insert("chmod".into(), json!(c.chmod));
            m.insert("link".into(), json!(c.link));
            m.insert("parents".into(), json!(c.parents));
            m.insert("excludes".into(), json!(c.excludes));
        }
        Instruction::Add(a) => {
            m.insert("sources".into(), json!(a.sources));
            m.insert("dest".into(), json!(a.dest));
            m.insert("contents".into(), contents(&a.contents));
            m.insert("chown".into(), json!(a.chown));
            m.insert("chmod".into(), json!(a.chmod));
            m.insert("link".into(), json!(a.link));
            m.insert("excludes".into(), json!(a.excludes));
            m.insert("keep_git_dir".into(), json!(a.keep_git_dir));
            m.insert("checksum".into(), json!(a.checksum));
            m.insert("unpack".into(), json!(a.unpack));
        }
        Instruction::Healthcheck(h) => {
            m.insert("test".into(), json!(h.test));
            m.insert("interval".into(), json!(h.interval.as_nanos()));
            m.insert("timeout".into(), json!(h.timeout.as_nanos()));
            m.insert("start_period".into(), json!(h.start_period.as_nanos()));
            m.insert("start_interval".into(), json!(h.start_interval.as_nanos()));
            m.insert("retries".into(), json!(h.retries));
        }
    }
    Value::Object(m)
}

fn typed_json(df: &Dockerfile) -> (Value, Value) {
    let meta: Vec<Value> = df
        .meta_args
        .iter()
        .map(|a| {
            let args: Vec<Value> = a.value.iter().map(|d| json!([d.key, d.value])).collect();
            json!({
                "cmd": a.keyword.to_lowercase(),
                "lines": lines(a.lines),
                "args": args,
            })
        })
        .collect();
    let stages: Vec<Value> = df
        .stages
        .iter()
        .map(|s| {
            let commands: Vec<Value> = s.instructions.iter().map(command_json).collect();
            json!({
                "name": s.name.clone().unwrap_or_default(),
                "base": s.base,
                "platform": s.platform.clone().unwrap_or_default(),
                "lines": lines(s.lines),
                "commands": commands,
            })
        })
        .collect();
    (Value::Array(meta), Value::Array(stages))
}

/// The golden stages with each `mount_error` message reduced to `true`:
/// messages are not compared, only that the mount fails.
fn normalize_golden_stages(v: &Value) -> Value {
    let mut v = v.clone();
    for s in v.as_array_mut().into_iter().flatten() {
        for c in s["commands"].as_array_mut().into_iter().flatten() {
            if c.get("mount_error").is_some() {
                c["mount_error"] = json!(true);
            }
        }
    }
    v
}

/// The first instruction that differs, for a readable failure.
fn first_difference(got: &Value, want: &Value) -> String {
    let empty = Vec::new();
    for (g, w) in got
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .zip(want.as_array().unwrap_or(&empty))
    {
        let gc = g["commands"].as_array().unwrap_or(&empty);
        let wc = w["commands"].as_array().unwrap_or(&empty);
        for (a, b) in gc.iter().zip(wc) {
            if a != b {
                return format!("instruction differs:\n  got  {a}\n  want {b}");
            }
        }
    }
    format!("stages differ:\n  got  {got}\n  want {want}")
}

/// Compares the typed instructions built from `ast` with BuildKit's.
fn compare_typed(ast: &Ast, golden: &Value) -> Option<String> {
    match (from_ast(ast), golden["ok"] == true) {
        (Ok(df), true) => {
            let (meta, stages) = typed_json(&df);
            let want_stages = normalize_golden_stages(&golden["stages"]);
            if meta != golden["meta_args"] {
                Some(format!(
                    "meta args differ:\n  got  {meta}\n  want {}",
                    golden["meta_args"]
                ))
            } else if stages != want_stages {
                Some(first_difference(&stages, &want_stages))
            } else {
                None
            }
        }
        (Ok(_), false) => Some(format!("BuildKit rejects it: {}", golden["error"])),
        (Err(e), true) => Some(format!("rejected, BuildKit accepts it: {e}")),
        (Err(e), false) => {
            if error_line_matches(golden, e.lines().map(|l| l.start)) {
                None
            } else {
                Some(format!(
                    "error line {:?} differs from BuildKit's {} ({})",
                    e.lines(),
                    golden["error_line"],
                    golden["error"]
                ))
            }
        }
    }
}

struct Outcome {
    passed: Vec<String>,
    failed: Vec<String>,
}

/// Compares the raw syntax tree and the typed instructions of every fixture
/// of a set with BuildKit's.
fn compare_set(set: &str, fixtures: &Path, flatten: bool) -> Outcome {
    let golden_dir = tests_dir().join("golden").join(set);
    let mut out = Outcome {
        passed: Vec::new(),
        failed: Vec::new(),
    };
    let mut entries: Vec<PathBuf> = fs::read_dir(&golden_dir)
        .unwrap_or_else(|e| panic!("{}: {e}", golden_dir.display()))
        .map(|e| e.expect("entry").path())
        .collect();
    entries.sort();
    for golden_path in entries {
        let golden = read_json(&golden_path);
        let file = golden["file"].as_str().expect("file").to_owned();
        let src_path = if flatten {
            // BuildKit's test files are flattened to `<set>_<case>`.
            let (dir, case) = file.split_once('_').expect("set_case");
            fixtures.join(dir).join(case).join("Dockerfile")
        } else {
            fixtures.join(&file)
        };
        let src =
            fs::read_to_string(&src_path).unwrap_or_else(|e| panic!("{}: {e}", src_path.display()));
        let raw_ok = !golden["ast"].as_array().expect("ast").is_empty();
        let result = parse_ast(&src);
        let problem = match (&result, raw_ok) {
            (Ok(ast), true) => {
                let got = ast_json(ast);
                if got != golden["ast"] {
                    Some(format!(
                        "raw AST differs:\n  got  {got}\n  want {}",
                        golden["ast"]
                    ))
                } else if ast.escape.to_string() != golden["escape"].as_str().unwrap_or("") {
                    Some(format!("escape {:?} differs", ast.escape))
                } else if ast.warnings.len() as u64 != golden["warnings"].as_u64().unwrap_or(0) {
                    Some(format!("{} warnings differ", ast.warnings.len()))
                } else {
                    compare_typed(ast, &golden)
                }
            }
            (Ok(_), false) => Some(format!("BuildKit rejects it: {}", golden["error"])),
            (Err(e), true) => Some(format!("rejected, BuildKit accepts it: {e}")),
            (Err(e), false) => {
                if error_line_matches(&golden, e.lines().map(|l| l.start)) {
                    None
                } else {
                    Some(format!(
                        "error line {:?} differs from BuildKit's {} ({})",
                        e.lines(),
                        golden["error_line"],
                        golden["error"]
                    ))
                }
            }
        };
        match problem {
            None => out.passed.push(file),
            Some(p) => out.failed.push(format!("{file}: {p}")),
        }
    }
    out
}

fn assert_all(set: &str, out: &Outcome, min: usize) {
    eprintln!(
        "{set}: {} of {} files match BuildKit",
        out.passed.len(),
        out.passed.len() + out.failed.len()
    );
    assert!(
        out.passed.len() + out.failed.len() >= min,
        "{set}: goldens missing"
    );
    assert!(out.failed.is_empty(), "{set}:\n{}", out.failed.join("\n"));
}

#[test]
fn kaniko_goldens() {
    let out = compare_set("kaniko", &tests_dir().join("fixtures/kaniko"), false);
    assert_all("kaniko", &out, 200);
}

#[test]
fn buildkit_goldens() {
    let out = compare_set(
        "buildkit",
        &tests_dir().join("fixtures/buildkit/parser"),
        true,
    );
    assert_all("buildkit", &out, 37);
}

#[test]
fn extra_goldens() {
    let out = compare_set("extra", &tests_dir().join("fixtures/extra"), false);
    assert_all("extra", &out, 60);
}

#[test]
fn lexer_vectors() {
    let golden = read_json(&tests_dir().join("golden/lex.json"));
    let env: BTreeMap<String, String> = golden["env"]
        .as_object()
        .expect("env")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("string").to_owned()))
        .collect();
    let vectors = golden["vectors"].as_array().expect("vectors");
    assert!(vectors.len() > 1000);
    let mut failed = Vec::new();
    for v in vectors {
        let c = &v["case"];
        let word = c["word"].as_str().expect("word");
        let escape = c["escape"]
            .as_str()
            .expect("escape")
            .chars()
            .next()
            .expect("char");
        let lex = Lexer::new(escape)
            .raw_quotes(c["raw_quotes"] == true)
            .raw_escapes(c["raw_escapes"] == true)
            .skip_unset_env(c["skip_unset"] == true)
            .skip_process_quotes(c["skip_quotes"] == true);
        let got = match lex.process_word_with_matches(word, &env) {
            Ok(m) => json!({
                "result": m.result,
                "matched": m.matched,
                "unmatched": m.unmatched,
            }),
            Err(_) => json!({"result": null, "matched": [], "unmatched": []}),
        };
        let words = lex.process_words(word, &env).ok();
        let want = json!({
            "result": v["result"],
            "matched": v["matched"],
            "unmatched": v["unmatched"],
        });
        if got != want || json!(words) != v["words"] {
            failed.push(format!(
                "{c}\n  got  {got} {words:?}\n  want {want} {}",
                v["words"]
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} lexer vectors differ:\n{}",
        failed.len(),
        vectors.len(),
        failed.join("\n")
    );
}
