//! Golden tests against BuildKit v0.34.0.
//!
//! `tests/golden/` holds BuildKit's output for the Dockerfiles in
//! `tests/fixtures/` and for generated shell-lexer cases, written by the Go
//! program in `tests/golden-gen/` (see its README). These tests compute the
//! same JSON from rnk-dockerfile and compare.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rnk_dockerfile::{Ast, Lexer, parse_ast};
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

struct Outcome {
    passed: Vec<String>,
    failed: Vec<String>,
}

/// Compares the raw syntax tree of every fixture of a set with BuildKit's.
fn compare_raw(set: &str, fixtures: &Path, flatten: bool) -> Outcome {
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
                    None
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
fn kaniko_raw_ast() {
    let out = compare_raw("kaniko", &tests_dir().join("fixtures/kaniko"), false);
    assert_all("kaniko raw", &out, 200);
}

#[test]
fn buildkit_raw_ast() {
    let out = compare_raw(
        "buildkit",
        &tests_dir().join("fixtures/buildkit/parser"),
        true,
    );
    assert_all("buildkit raw", &out, 37);
}

#[test]
fn extra_raw_ast() {
    let out = compare_raw("extra", &tests_dir().join("fixtures/extra"), false);
    assert_all("extra raw", &out, 60);
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
