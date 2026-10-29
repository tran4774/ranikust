// Derived from moby/patternmatcher, https://github.com/moby/patternmatcher,
// patternmatcher_test.go at tag v0.6.1 (commit 5a6d8429a19b).
// Copyright 2012-2017 Docker, Inc. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: Rust tests over the same tables; the deprecated
// `Matches` API is checked through `MatchesOrParentMatches`.

use super::*;

/// The package-level `Matches`/`MatchesOrParentMatches` helpers: clean the
/// path, and never exclude `.`.
fn matches(file: &str, patterns: &[&str]) -> Result<bool, Error> {
    let pm = Matcher::new(patterns)?;
    let file = golang::clean(file.as_bytes());
    if file == b"." {
        return Ok(false);
    }
    pm.matches_or_parent_matches(file)
}

/// `MatchesUsingParentResults` called on each parent directory in turn,
/// then on `text`, as a walk does.
fn matches_along_walk(pm: &Matcher, text: &str) -> bool {
    let parent = golang::dir(text.as_bytes());
    let mut info = MatchInfo::default();
    if parent != b"." {
        let mut end = 0;
        for part in parent.split(|&b| b == b'/') {
            end += part.len();
            (_, info) = pm.matches_using_parent(&parent[..end], &info).unwrap();
            end += 1;
        }
    }
    pm.matches_using_parent(text, &info).unwrap().0
}

#[test]
fn wildcard_matches() {
    assert_eq!(matches("fileutils.go", &["*"]), Ok(true));
}

#[test]
fn pattern_matches() {
    assert_eq!(matches("fileutils.go", &["*.go"]), Ok(true));
}

#[test]
fn exclusion_pattern_matches_pattern_before() {
    assert_eq!(
        matches("fileutils.go", &["!fileutils.go", "*.go"]),
        Ok(true)
    );
}

#[test]
fn pattern_matches_folder_exclusions() {
    assert_eq!(
        matches("docs/README.md", &["docs", "!docs/README.md"]),
        Ok(false)
    );
    assert_eq!(
        matches("docs/README.md", &["docs/", "!docs/README.md"]),
        Ok(false)
    );
    assert_eq!(
        matches("docs/README.md", &["docs/*", "!docs/README.md"]),
        Ok(false)
    );
}

#[test]
fn exclusion_pattern_matches_pattern_after() {
    assert_eq!(
        matches("fileutils.go", &["*.go", "!fileutils.go"]),
        Ok(false)
    );
}

#[test]
fn exclusion_pattern_matches_whole_directory() {
    assert_eq!(matches(".", &["*.go"]), Ok(false));
}

#[test]
fn single_exclamation_error() {
    assert_eq!(
        matches("fileutils.go", &["!"]),
        Err(Error::IllegalExclusion)
    );
}

#[test]
fn matches_with_no_patterns() {
    assert_eq!(matches("/any/path/there", &[]), Ok(false));
}

#[test]
fn matches_with_malformed_patterns() {
    assert!(matches!(
        matches("/any/path/there", &["["]),
        Err(Error::BadPattern { .. })
    ));
}

const TESTS: &[(&str, &str, bool)] = &[
    ("**", "file", true),
    ("**", "file/", true),
    ("**/", "file", true), // weird one
    ("**/", "file/", true),
    ("**", "/", true),
    ("**/", "/", true),
    ("**", "dir/file", true),
    ("**/", "dir/file", true),
    ("**", "dir/file/", true),
    ("**/", "dir/file/", true),
    ("**/**", "dir/file", true),
    ("**/**", "dir/file/", true),
    ("dir/**", "dir/file", true),
    ("dir/**", "dir/file/", true),
    ("dir/**", "dir/dir2/file", true),
    ("dir/**", "dir/dir2/file/", true),
    ("**/dir", "dir", true),
    ("**/dir", "dir/file", true),
    ("**/dir2/*", "dir/dir2/file", true),
    ("**/dir2/*", "dir/dir2/file/", true),
    ("**/dir2/**", "dir/dir2/dir3/file", true),
    ("**/dir2/**", "dir/dir2/dir3/file/", true),
    ("**file", "file", true),
    ("**file", "dir/file", true),
    ("**/file", "dir/file", true),
    ("**file", "dir/dir/file", true),
    ("**/file", "dir/dir/file", true),
    ("**/file*", "dir/dir/file", true),
    ("**/file*", "dir/dir/file.txt", true),
    ("**/file*txt", "dir/dir/file.txt", true),
    ("**/file*.txt", "dir/dir/file.txt", true),
    ("**/file*.txt*", "dir/dir/file.txt", true),
    ("**/**/*.txt", "dir/dir/file.txt", true),
    ("**/**/*.txt2", "dir/dir/file.txt", false),
    ("**/*.txt", "file.txt", true),
    ("**/**/*.txt", "file.txt", true),
    ("a**/*.txt", "a/file.txt", true),
    ("a**/*.txt", "a/dir/file.txt", true),
    ("a**/*.txt", "a/dir/dir/file.txt", true),
    ("a/*.txt", "a/dir/file.txt", false),
    ("a/*.txt", "a/file.txt", true),
    ("a/*.txt**", "a/file.txt", true),
    ("a[b-d]e", "ae", false),
    ("a[b-d]e", "ace", true),
    ("a[b-d]e", "aae", false),
    ("a[^b-d]e", "aze", true),
    (".*", ".foo", true),
    (".*", "foo", false),
    ("abc.def", "abcdef", false),
    ("abc.def", "abc.def", true),
    ("abc.def", "abcZdef", false),
    ("abc?def", "abcZdef", true),
    ("abc?def", "abcdef", false),
    ("a\\\\", "a\\", true),
    ("**/foo/bar", "foo/bar", true),
    ("**/foo/bar", "dir/foo/bar", true),
    ("**/foo/bar", "dir/dir2/foo/bar", true),
    ("abc/**", "abc", false),
    ("abc/**", "abc/def", true),
    ("abc/**", "abc/def/ghi", true),
    ("**/.foo", ".foo", true),
    ("**/.foo", "bar.foo", false),
    ("a(b)c/def", "a(b)c/def", true),
    ("a(b)c/def", "a(b)c/xyz", false),
    ("a.|)$(}+{bc", "a.|)$(}+{bc", true),
    (
        "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
        "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
        true,
    ),
    (
        "dist/*.whl",
        "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
        true,
    ),
    // Added upstream on non-Windows systems only.
    ("a\\*b", "a*b", true),
];

const MULTI_PATTERN_TESTS: &[(&[&str], &str, bool)] = &[
    (&["**", "!util/docker/web"], "util/docker/web/foo", false),
    (
        &["**", "!util/docker/web", "util/docker/web/foo"],
        "util/docker/web/foo",
        true,
    ),
    (
        &[
            "**",
            "!dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
        ],
        "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
        false,
    ),
    (
        &["**", "!dist/*.whl"],
        "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
        false,
    ),
];

fn all_cases() -> impl Iterator<Item = (Vec<&'static str>, &'static str, bool)> {
    TESTS
        .iter()
        .map(|&(p, text, pass)| (vec![p], text, pass))
        .chain(
            MULTI_PATTERN_TESTS
                .iter()
                .map(|&(p, text, pass)| (p.to_vec(), text, pass)),
        )
}

#[test]
fn matches_or_parent_matches_table() {
    for (patterns, text, pass) in all_cases() {
        let pm = Matcher::new(&patterns).unwrap();
        assert_eq!(
            pm.matches_or_parent_matches(text),
            Ok(pass),
            "patterns={patterns:?} text={text:?}"
        );
    }
}

#[test]
fn matches_using_parent_results_table() {
    for (patterns, text, pass) in all_cases() {
        let pm = Matcher::new(&patterns).unwrap();
        assert_eq!(
            matches_along_walk(&pm, text),
            pass,
            "patterns={patterns:?} text={text:?}"
        );
    }
}

#[test]
fn matches_using_parent_results_no_context_table() {
    for (patterns, text, pass) in all_cases() {
        let pm = Matcher::new(&patterns).unwrap();
        let (m, _) = pm
            .matches_using_parent(text, &MatchInfo::default())
            .unwrap();
        assert_eq!(m, pass, "patterns={patterns:?} text={text:?}");
    }
}

#[test]
fn clean_patterns() {
    let pm = Matcher::new(&["docs", "config"]).unwrap();
    assert_eq!(pm.patterns.len(), 2);
}

#[test]
fn clean_patterns_strip_empty_patterns() {
    let pm = Matcher::new(&["docs", "config", ""]).unwrap();
    assert_eq!(pm.patterns.len(), 2);
}

#[test]
fn clean_patterns_exception_flag() {
    assert!(
        Matcher::new(&["docs", "!docs/README.md"])
            .unwrap()
            .has_exclusions()
    );
}

#[test]
fn clean_patterns_leading_space_trimmed() {
    assert!(
        Matcher::new(&["docs", "  !docs/README.md"])
            .unwrap()
            .has_exclusions()
    );
}

#[test]
fn clean_patterns_trailing_space_trimmed() {
    assert!(
        Matcher::new(&["docs", "!docs/README.md  "])
            .unwrap()
            .has_exclusions()
    );
}

#[test]
fn clean_patterns_error_single_exception() {
    assert_eq!(Matcher::new(&["!"]).unwrap_err(), Error::IllegalExclusion);
}

/// Upstream's `matchTests`, taken from Go's filepath tests, run through the
/// package-level helper.
#[test]
fn match_table() {
    let cases: &[(&str, &str, Option<bool>)] = &[
        ("abc", "abc", Some(true)),
        ("*", "abc", Some(true)),
        ("*c", "abc", Some(true)),
        ("a*", "a", Some(true)),
        ("a*", "abc", Some(true)),
        ("a*", "ab/c", Some(true)),
        ("a*/b", "abc/b", Some(true)),
        ("a*/b", "a/c/b", Some(false)),
        ("a*b*c*d*e*/f", "axbxcxdxe/f", Some(true)),
        ("a*b*c*d*e*/f", "axbxcxdxexxx/f", Some(true)),
        ("a*b*c*d*e*/f", "axbxcxdxe/xxx/f", Some(false)),
        ("a*b*c*d*e*/f", "axbxcxdxexxx/fff", Some(false)),
        ("a*b?c*x", "abxbbxdbxebxczzx", Some(true)),
        ("a*b?c*x", "abxbbxdbxebxczzy", Some(false)),
        ("ab[c]", "abc", Some(true)),
        ("ab[b-d]", "abc", Some(true)),
        ("ab[e-g]", "abc", Some(false)),
        ("ab[^c]", "abc", Some(false)),
        ("ab[^b-d]", "abc", Some(false)),
        ("ab[^e-g]", "abc", Some(true)),
        ("a\\*b", "a*b", Some(true)),
        ("a\\*b", "ab", Some(false)),
        ("a?b", "a☺b", Some(true)),
        ("a[^a]b", "a☺b", Some(true)),
        ("a???b", "a☺b", Some(false)),
        ("a[^a][^a][^a]b", "a☺b", Some(false)),
        ("[a-ζ]*", "α", Some(true)),
        ("*[a-ζ]", "A", Some(false)),
        ("a?b", "a/b", Some(false)),
        ("a*b", "a/b", Some(false)),
        ("[\\]a]", "]", Some(true)),
        ("[\\-]", "-", Some(true)),
        ("[x\\-]", "x", Some(true)),
        ("[x\\-]", "-", Some(true)),
        ("[x\\-]", "z", Some(false)),
        ("[\\-x]", "x", Some(true)),
        ("[\\-x]", "-", Some(true)),
        ("[\\-x]", "a", Some(false)),
        ("[]a]", "]", None),
        ("[-]", "-", None),
        ("[x-]", "x", None),
        ("[x-]", "-", None),
        ("[x-]", "z", None),
        ("[-x]", "x", None),
        ("[-x]", "-", None),
        ("[-x]", "a", None),
        ("\\", "a", None),
        ("[a-b-c]", "a", None),
        ("[", "a", None),
        ("[^", "a", None),
        ("[^bc", "a", None),
        ("a[", "a", None), // was nil but IMO its wrong
        ("a[", "ab", None),
        ("*x", "xxx", Some(true)),
    ];
    for (pattern, s, want) in cases {
        let got = matches(s, &[pattern]);
        match want {
            Some(want) => assert_eq!(got, Ok(*want), "Match({pattern:?}, {s:?})"),
            None => assert!(
                matches!(got, Err(Error::BadPattern { .. })),
                "Match({pattern:?}, {s:?}) = {got:?}"
            ),
        }
    }
}

/// Upstream's `compileTests` (for the `/` separator): the match type and,
/// for regexps, the Go regexp.
#[test]
fn compile_table() {
    let cases: &[(&str, &str, &str)] = &[
        ("*", "regexp", "^[^/]*$"),
        ("file*", "regexp", "^file[^/]*$"),
        ("*file", "regexp", "^[^/]*file$"),
        ("a*/b", "regexp", "^a[^/]*/b$"),
        ("**", "suffix", ""),
        ("**/**", "regexp", "^(.*/)?.*$"),
        ("dir/**", "prefix", ""),
        ("**/dir", "suffix", ""),
        ("**/dir2/*", "regexp", "^(.*/)?dir2/[^/]*$"),
        ("**/dir2/**", "regexp", "^(.*/)?dir2/.*$"),
        ("**file", "suffix", ""),
        ("**/file*txt", "regexp", "^(.*/)?file[^/]*txt$"),
        ("**/**/*.txt", "regexp", "^(.*/)?(.*/)?[^/]*\\.txt$"),
        ("a[b-d]e", "regexp", "^a[b-d]e$"),
        (".*", "regexp", "^\\.[^/]*$"),
        ("abc.def", "exact", ""),
        ("abc?def", "regexp", "^abc[^/]def$"),
        ("**/foo/bar", "suffix", ""),
        ("a(b)c/def", "exact", ""),
        ("a.|)$(}+{bc", "exact", ""),
        (
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            "exact",
            "",
        ),
    ];
    for (pattern, want_type, want_re) in cases {
        let cleaned = golang::clean(pattern.as_bytes());
        let (kind, re) = compile(&cleaned).unwrap();
        let got_type = match kind {
            MatchType::Exact => "exact",
            MatchType::Prefix => "prefix",
            MatchType::Suffix => "suffix",
            MatchType::Regexp(_) => "regexp",
        };
        assert_eq!(got_type, *want_type, "{pattern}");
        if *want_type == "regexp" {
            assert_eq!(re, *want_re, "{pattern}");
        }
    }
}

/// Regression test for moby/moby#52203: a pattern that passes `New` but
/// fails to compile reports the error on every call.
#[test]
fn malformed_pattern_errors_on_every_call() {
    let pm = Matcher::new(&["[Local-Only]/"]).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            pm.matches_or_parent_matches("x"),
            Err(Error::BadPattern { .. })
        ));
    }
}

// Behaviour beyond the upstream tests, each checked against Go.

#[test]
fn unescaped_regexp_operators() {
    // `|`, `{` and `}` reach Go's regexp unescaped.
    let pm = Matcher::new(&["a|b*"]).unwrap();
    assert_eq!(pm.matches_or_parent_matches("azzz/q"), Ok(true));
    assert_eq!(pm.matches_or_parent_matches("zb"), Ok(true));
    let pm = Matcher::new(&["a{2}*"]).unwrap();
    assert_eq!(pm.matches_or_parent_matches("aa"), Ok(true));
    assert_eq!(pm.matches_or_parent_matches("a{2}"), Ok(false));
    assert_eq!(
        Matcher::new(&["x{*"])
            .unwrap()
            .matches_or_parent_matches("x{y"),
        Ok(true)
    );
    assert!(
        Matcher::new(&["a{1001}*"])
            .unwrap()
            .matches_or_parent_matches("a")
            .is_err()
    );
}

#[test]
fn class_quirks() {
    // `[a*]` becomes `^[a[^/]*]$`: a class of `a[^/`, starred, then `]`.
    let pm = Matcher::new(&["[a*]"]).unwrap();
    assert_eq!(pm.matches_or_parent_matches("]"), Ok(true));
    assert_eq!(pm.matches_or_parent_matches("a"), Ok(false));
    // `[!b]` is a class of `!` and `b`; only `[^b]` negates.
    let pm = Matcher::new(&["[!b].md"]).unwrap();
    assert_eq!(pm.matches_or_parent_matches("a.md"), Ok(false));
    assert_eq!(pm.matches_or_parent_matches("!.md"), Ok(true));
}

#[test]
fn byte_order_mark_and_invalid_bytes() {
    // The scanner skips a leading BOM in the regexp, but exact, prefix and
    // suffix matches compare the cleaned bytes.
    assert_eq!(
        Matcher::new(&["\u{feff}fo*"])
            .unwrap()
            .matches_or_parent_matches("foo"),
        Ok(true)
    );
    assert_eq!(
        Matcher::new(&["\u{feff}**"])
            .unwrap()
            .matches_or_parent_matches("x"),
        Ok(false)
    );
    let pm = Matcher::new(&[b"\x80a".as_slice()]).unwrap();
    assert_eq!(pm.matches_or_parent_matches(b"\x80a"), Ok(true));
    assert_eq!(pm.matches_or_parent_matches("\u{fffd}a"), Ok(false));
    let pm = Matcher::new(&[b"\x80a*".as_slice()]).unwrap();
    assert_eq!(pm.matches_or_parent_matches(b"\x80abc"), Ok(true));
    assert_eq!(pm.matches_or_parent_matches("\u{fffd}abc"), Ok(true));
}

#[test]
fn suffix_match_ignores_newlines_but_regexps_do_not() {
    assert_eq!(
        Matcher::new(&["**/x"])
            .unwrap()
            .matches_or_parent_matches("a\nb/x"),
        Ok(true)
    );
    assert_eq!(
        Matcher::new(&["**/*x"])
            .unwrap()
            .matches_or_parent_matches("a\nb/x"),
        Ok(false)
    );
}

#[test]
fn late_syntax_errors_surface_when_matching() {
    // `filepath.Match` stops validating at the first chunk that fails.
    let pm = Matcher::new(&["a*["]).unwrap();
    assert!(pm.matches_or_parent_matches("x").is_err());
    // A pattern that is never evaluated never fails.
    let pm = Matcher::new(&["a", "![z-a]*"]).unwrap();
    assert_eq!(pm.matches_or_parent_matches("b"), Ok(false));
    assert!(pm.matches_or_parent_matches("a").is_err());
}

#[test]
fn walk_and_parent_matches_can_differ() {
    // BuildKit's walk skips `!dir` for `dir` (nothing matched yet) and does
    // not reconsider it through the parent for `dir/f` [open: Q-050].
    let pm = Matcher::new(&["dir/f", "!dir"]).unwrap();
    assert_eq!(pm.matches_or_parent_matches("dir/f"), Ok(false));
    assert!(matches_along_walk(&pm, "dir/f"));
}

#[test]
fn match_info_from_another_matcher() {
    let a = Matcher::new(&["a", "b"]).unwrap();
    let b = Matcher::new(&["a"]).unwrap();
    let (_, info) = a.matches_using_parent("x", &MatchInfo::default()).unwrap();
    assert_eq!(
        b.matches_using_parent("x/y", &info),
        Err(Error::MatchInfoMismatch)
    );
}
