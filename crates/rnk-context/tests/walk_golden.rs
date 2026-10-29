//! The real filesystem walk against BuildKit's filtered walk.
//!
//! `rnk-ignore`'s golden `walk.txt` records, for a 51-entry tree and several
//! hundred pattern lists, which entries `fsutil.Walk` (BuildKit v0.34.0)
//! reports. Here the tree is built on disk and walked by `rnk-context`; the
//! reported paths must match BuildKit's, in the same order.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use rnk_context::{ContextSource as _, DirSource};
use rnk_ignore::Matcher;

const WALK: &str = include_str!("../../rnk-ignore/tests/golden/walk.txt");

/// `%XX`-escaped bytes; a lone `%` is the empty string.
fn decode(field: &str) -> Vec<u8> {
    if field == "%" {
        return Vec::new();
    }
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

#[test]
fn walk_matches_buildkit_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let mut entries: Vec<Vec<u8>> = Vec::new();
    let mut cases = 0;
    let mut failures = Vec::new();
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    let mut ctx = None;
    for line in WALK.lines().filter(|l| !l.starts_with('#')) {
        let (keyword, rest) = line.split_once(' ').unwrap_or((line, ""));
        match keyword {
            "entry" => {
                let mut path = decode(rest);
                let host = tmp.path().join(OsStr::from_bytes(&path));
                if path.last() == Some(&b'/') {
                    path.pop();
                    std::fs::create_dir_all(&host).unwrap();
                } else {
                    std::fs::write(&host, b"").unwrap();
                }
                entries.push(path);
            }
            "case" => patterns.clear(),
            "pattern" => patterns.push(decode(rest)),
            "emit" | "error" => {
                cases += 1;
                let ctx = ctx.get_or_insert_with(|| DirSource::new(tmp.path()).open().unwrap());
                let expected: Option<Vec<PathBuf>> = (keyword == "emit").then(|| {
                    entries
                        .iter()
                        .zip(rest.bytes())
                        .filter(|&(_, bit)| bit == b'1')
                        .map(|(p, _)| PathBuf::from(OsStr::from_bytes(p)))
                        .collect()
                });
                let got = Matcher::new(&patterns)
                    .map_err(rnk_context::Error::from)
                    .and_then(|m| {
                        ctx.walk(Some(&m))
                            .map(|e| e.map(|e| e.path))
                            .collect::<Result<Vec<_>, _>>()
                    });
                let ok = match (&expected, &got) {
                    (Some(want), Ok(got)) => want == got,
                    (None, Err(_)) => true,
                    _ => false,
                };
                if !ok {
                    failures.push(format!(
                        "patterns {:?}: got {got:?}, BuildKit {expected:?}",
                        patterns
                            .iter()
                            .map(|p| String::from_utf8_lossy(p).into_owned())
                            .collect::<Vec<_>>()
                    ));
                }
            }
            other => panic!("unknown golden record {other:?}"),
        }
    }
    assert!(entries.len() > 40, "only {} entries read", entries.len());
    assert!(cases > 300, "only {cases} cases read");
    assert!(
        failures.is_empty(),
        "{} of {cases} walks differ from BuildKit:\n{}",
        failures.len(),
        failures[..failures.len().min(10)].join("\n")
    );
}
