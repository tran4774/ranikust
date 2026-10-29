//! BuildKit's own parser test files (`frontend/dockerfile/parser/testfiles`
//! at v0.34.0): each `Dockerfile` must parse, and its dump must equal the
//! `result` file next to it; each file in `testfiles-negative` must fail.

use std::fs;
use std::path::{Path, PathBuf};

fn dirs(sub: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/buildkit/parser")
        .join(sub);
    let mut out: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("{}: {e}", root.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    out.sort();
    out
}

#[test]
fn testfiles_dump_matches_result() {
    let mut failures = Vec::new();
    let all = dirs("testfiles");
    assert!(all.len() >= 30, "fixtures missing");
    for dir in &all {
        let src = fs::read_to_string(dir.join("Dockerfile")).expect("Dockerfile");
        let expected = fs::read_to_string(dir.join("result")).expect("result");
        match rnk_dockerfile::parse_ast(&src) {
            Ok(ast) => {
                let got = ast.dump() + "\n";
                if got != expected {
                    failures.push(format!(
                        "{}:\n--- expected\n{expected}--- got\n{got}",
                        dir.display()
                    ));
                }
            }
            Err(e) => failures.push(format!("{}: {e}", dir.display())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn negative_testfiles_fail() {
    for dir in dirs("testfiles-negative") {
        let src = fs::read_to_string(dir.join("Dockerfile")).expect("Dockerfile");
        let e = rnk_dockerfile::parse_ast(&src).expect_err(&dir.display().to_string());
        assert!(e.lines().is_some(), "{}: {e}", dir.display());
    }
}
