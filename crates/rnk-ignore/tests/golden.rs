//! Differential tests against goldens written by the Go code rnk-ignore
//! ports (`gen/main.go`, regenerated with `gen/gen.sh`): moby
//! `patternmatcher` v0.6.1, `ignorefile.ReadAll`, and BuildKit's filtered
//! context walk from `fsutil`.

use rnk_ignore::{Error, MatchInfo, Matcher, read_patterns};

const MATCHER: &str = include_str!("golden/matcher.txt");
const READALL: &str = include_str!("golden/readall.txt");
const WALK: &str = include_str!("golden/walk.txt");

/// Decodes a golden field: `%XX` escapes, and a lone `%` for "".
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

/// The lines of a golden without comments, split into a keyword and its
/// fields.
fn records(golden: &str) -> impl Iterator<Item = (&str, Vec<&str>)> {
    golden.lines().filter(|l| !l.starts_with('#')).map(|l| {
        let mut fields = l.split(' ');
        let keyword = fields.next().unwrap();
        (keyword, fields.collect())
    })
}

fn code(result: &Result<bool, Error>) -> &'static str {
    match result {
        Ok(false) => "0",
        Ok(true) => "1",
        Err(_) => "E",
    }
}

/// `MatchesUsingParentResults` on each prefix of `path`, as the generator
/// does.
fn walk_chain(m: &Matcher, path: &[u8]) -> Result<bool, Error> {
    let mut info = MatchInfo::default();
    let mut matched = false;
    let mut end = 0;
    for (i, part) in path.split(|&b| b == b'/').enumerate() {
        if i > 0 {
            end += 1;
        }
        end += part.len();
        (matched, info) = m.matches_using_parent(&path[..end], &info)?;
    }
    Ok(matched)
}

fn show(bytes: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(bytes))
}

#[test]
fn matcher_matches_go() {
    let mut failures = Vec::new();
    let mut cells = 0;
    let mut name = String::new();
    let mut file: Option<Vec<u8>> = None;
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    let mut matcher: Option<Matcher> = None;
    for (keyword, fields) in records(MATCHER) {
        match keyword {
            "case" => {
                name = String::from_utf8_lossy(&decode(fields[0])).into_owned();
                file = None;
                patterns.clear();
                matcher = None;
            }
            "file" => file = Some(decode(fields[0])),
            "pattern" => patterns.push(decode(fields[0])),
            "new" => {
                if let Some(content) = &file {
                    let read = read_patterns(content).unwrap();
                    if read != patterns {
                        failures.push(format!(
                            "{name}: read_patterns gave {read:?}, Go {patterns:?}"
                        ));
                    }
                }
                let built = Matcher::new(&patterns);
                if built.is_ok() != (fields[0] == "ok") {
                    failures.push(format!(
                        "{name}: Matcher::new = {built:?}, Go new {}",
                        fields[0]
                    ));
                }
                matcher = built.ok();
            }
            "path" => {
                let Some(m) = &matcher else { continue };
                cells += 1;
                let path = decode(fields[0]);
                let got = (
                    code(&m.matches_or_parent_matches(&path)),
                    code(&walk_chain(m, &path)),
                );
                if got != (fields[1], fields[2]) {
                    let patterns: Vec<String> = patterns.iter().map(|p| show(p)).collect();
                    failures.push(format!(
                        "{name}: patterns [{}] path {}: got {got:?}, Go ({}, {})",
                        patterns.join(", "),
                        show(&path),
                        fields[1],
                        fields[2]
                    ));
                }
            }
            other => panic!("unknown golden record {other:?}"),
        }
    }
    assert!(cells > 10_000, "only {cells} cells read");
    assert!(
        failures.is_empty(),
        "{} of {cells} cells differ from Go:\n{}",
        failures.len(),
        failures[..failures.len().min(40)].join("\n")
    );
}

#[test]
fn read_patterns_matches_go() {
    let mut failures = Vec::new();
    let mut files = 0;
    let mut content = Vec::new();
    let mut want: Vec<Vec<u8>> = Vec::new();
    for (keyword, fields) in records(READALL) {
        match keyword {
            "file" => {
                content = decode(fields[0]);
                want.clear();
            }
            "pattern" => want.push(decode(fields[0])),
            "end" => {
                files += 1;
                let got = read_patterns(&content).unwrap();
                if got != want {
                    failures.push(format!("{}: got {got:?}, Go {want:?}", show(&content)));
                }
            }
            other => panic!("unknown golden record {other:?}"),
        }
    }
    assert!(files > 500, "only {files} files read");
    assert!(
        failures.is_empty(),
        "{} of {files} files differ from Go:\n{}",
        failures.len(),
        failures[..failures.len().min(40)].join("\n")
    );
}

/// A directory visited by the walk: fsutil's `visitedDir`.
struct Visited {
    path_with_sep: Vec<u8>,
    info: MatchInfo,
    entry: usize,
    reported: bool,
}

/// BuildKit's filtered walk (fsutil `filterFS.Walk` with exclude patterns
/// only) over `entries`, given in walk order: which entries it reports.
fn walk(m: &Matcher, entries: &[(Vec<u8>, bool)]) -> Result<Vec<bool>, Error> {
    let mut reported = vec![false; entries.len()];
    let mut parents: Vec<Visited> = Vec::new();
    let mut skipping: Option<Vec<u8>> = None;
    for (i, (path, is_dir)) in entries.iter().enumerate() {
        let with_sep = [path.as_slice(), b"/"].concat();
        if let Some(prefix) = &skipping {
            if path.starts_with(prefix) {
                continue;
            }
            skipping = None;
        }
        while parents
            .last()
            .is_some_and(|p| !path.starts_with(&p.path_with_sep))
        {
            parents.pop();
        }
        let parent_info = parents.last().map(|p| p.info.clone()).unwrap_or_default();
        let (excluded, info) = m.matches_using_parent(path, &parent_info)?;
        if excluded && *is_dir && !m.may_reinclude_below(path) {
            skipping = Some(with_sep);
            continue;
        }
        if !excluded {
            // Report the directories above that were held back first.
            for p in parents.iter_mut().filter(|p| !p.reported) {
                p.reported = true;
                reported[p.entry] = true;
            }
            reported[i] = true;
        }
        if *is_dir {
            parents.push(Visited {
                path_with_sep: with_sep,
                info,
                entry: i,
                reported: !excluded,
            });
        }
    }
    Ok(reported)
}

#[test]
fn walk_matches_buildkit() {
    let mut entries: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut failures = Vec::new();
    let mut cases = 0;
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    for (keyword, fields) in records(WALK) {
        match keyword {
            "entry" => {
                let mut path = decode(fields[0]);
                let is_dir = path.last() == Some(&b'/');
                if is_dir {
                    path.pop();
                }
                entries.push((path, is_dir));
            }
            "case" => patterns.clear(),
            "pattern" => patterns.push(decode(fields[0])),
            "emit" | "error" => {
                cases += 1;
                let got = Matcher::new(&patterns)
                    .and_then(|m| walk(&m, &entries))
                    .map(|bits| {
                        bits.iter()
                            .map(|&b| if b { '1' } else { '0' })
                            .collect::<String>()
                    });
                let ok = match (keyword, &got) {
                    ("emit", Ok(bits)) => bits == fields[0],
                    ("error", Err(_)) => true,
                    _ => false,
                };
                if !ok {
                    let patterns: Vec<String> = patterns.iter().map(|p| show(p)).collect();
                    failures.push(format!(
                        "patterns [{}]: got {got:?}, BuildKit {keyword} {fields:?}",
                        patterns.join(", ")
                    ));
                }
            }
            other => panic!("unknown golden record {other:?}"),
        }
    }
    assert!(cases > 300, "only {cases} cases read");
    assert!(
        failures.is_empty(),
        "{} of {cases} walks differ from BuildKit:\n{}",
        failures.len(),
        failures[..failures.len().min(20)].join("\n")
    );
}
