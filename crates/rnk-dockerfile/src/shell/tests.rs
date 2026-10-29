// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/shell/lex_test.go at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: the Go test tables rewritten as Rust tests.

use std::collections::BTreeMap;

use super::*;

/// BuildKit's `EnvsFromSlice`: `k=v` entries, a later entry wins, an entry
/// without `=` sets the empty string.
fn envs(entries: &[&str]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|e| match e.split_once('=') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => ((*e).to_owned(), String::new()),
        })
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

const ENV_VAR_TEST: &str = include_str!("../../tests/fixtures/buildkit/shell/envVarTest");
const WORDS_TEST: &str = include_str!("../../tests/fixtures/buildkit/shell/wordsTest");

#[test]
fn env_var_test_vectors() {
    let lex = Lexer::new('\\');
    let env = envs(&["PWD=/home", "SHELL=bash", "KOREAN=한국어", "NULL="]);
    let mut n = 0;
    for (i, line) in ENV_VAR_TEST.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.trim().split('|').collect();
        assert_eq!(parts.len(), 3, "line {}", i + 1);
        let (platform, source, expected) = (parts[0].trim(), parts[1].trim(), parts[2].trim());
        if platform == "W" {
            continue;
        }
        n += 1;
        let got = lex.process_word(source, &env);
        if expected == "error" {
            assert!(got.is_err(), "line {}: {source:?} gave {got:?}", i + 1);
        } else {
            assert_eq!(got.as_deref(), Ok(expected), "line {}: {source:?}", i + 1);
        }
    }
    assert!(n >= 228, "only {n} vectors");
}

#[test]
fn words_test_vectors() {
    let lex = Lexer::new('\\');
    let mut env: Vec<String> = Vec::new();
    for (i, line) in WORDS_TEST.lines().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("ENV ") {
            env.push(rest.trim_start_matches(' ').to_owned());
            continue;
        }
        let (test, expected) = line.split_once('|').expect("one |");
        let test = test.trim();
        let expected: Vec<&str> = expected.trim_start_matches(' ').split(',').collect();
        let env_refs: Vec<&str> = env.iter().map(String::as_str).collect();
        let got = lex
            .process_words(test, &envs(&env_refs))
            .unwrap_or_else(|_| vec!["error".to_owned()]);
        assert_eq!(got, expected, "line {}: {test:?}", i + 1);
    }
}

#[test]
fn mandatory_env_vars() {
    let lex = Lexer::new('\\');
    let set_envs = envs(&["VAR=plain", "ARG=x"]);
    let empty_envs = envs(&["VAR=", "ARG=x"]);
    let unset_envs = envs(&["ARG=x"]);
    let no_empty = "${VAR:?message here$ARG}";
    let no_unset = "${VAR?message here$ARG}";

    assert_eq!(lex.process_word(no_empty, &set_envs).unwrap(), "plain");
    let e = lex.process_word(no_empty, &empty_envs).unwrap_err();
    assert!(e.to_string().contains("message herex"), "{e}");
    let e = lex.process_word(no_empty, &unset_envs).unwrap_err();
    assert!(e.to_string().contains("message herex"), "{e}");

    assert_eq!(lex.process_word(no_unset, &set_envs).unwrap(), "plain");
    assert_eq!(lex.process_word(no_unset, &empty_envs).unwrap(), "");
    let e = lex.process_word(no_unset, &unset_envs).unwrap_err();
    assert!(e.to_string().contains("message herex"), "{e}");
}

#[test]
fn raw_escapes() {
    let env = envs(&["FOO=bar"]);
    let cases: &[(char, &str, &str)] = &[
        ('\\', r"C:\hello\world", r"C:\hello\world"),
        ('\\', r#""C:\hello\world""#, r#""C:\hello\world""#),
        ('\\', r#""a\"b" "a\\b" "\$FOO""#, r#""a\"b" "a\\b" "\$FOO""#),
        ('\\', r"\$FOO $FOO a\ b 'x\y'", r"\$FOO bar a\ b 'x\y'"),
        ('`', "C:\\hello `$FOO $FOO", "C:\\hello `$FOO bar"),
        ('`', "\"a`tb\" \"a`\"b\"", "\"a`tb\" \"a`\"b\""),
    ];
    for &(escape, input, expected) in cases {
        let lex = Lexer::new(escape).raw_quotes(true).raw_escapes(true);
        assert_eq!(
            lex.process_word(input, &env).unwrap(),
            expected,
            "{input:?}"
        );
    }
}

#[test]
fn raw_quotes_pattern_escapes() {
    let env = envs(&[r"FOO=a\bc", "BAR=ok"]);
    for input in [
        r#"$BAR ${FOO#"a\b"}"#,
        r"$BAR ${FOO#'a\b'}",
        r#"$BAR ${FOO/"a\b"/x}"#,
    ] {
        let lex = Lexer::new('\\').raw_quotes(true).raw_escapes(true);
        assert_eq!(
            lex.process_word(input, &env).unwrap(),
            r"ok a\bc",
            "{input:?}"
        );
    }
}

struct Case {
    input: &'static str,
    env: &'static [&'static str],
    expected: Option<&'static str>,
    matched: &'static [&'static str],
    unmatched: &'static [&'static str],
}

const fn ok(
    input: &'static str,
    env: &'static [&'static str],
    expected: &'static str,
    matched: &'static [&'static str],
    unmatched: &'static [&'static str],
) -> Case {
    Case {
        input,
        env,
        expected: Some(expected),
        matched,
        unmatched,
    }
}

const fn err(input: &'static str, env: &'static [&'static str]) -> Case {
    Case {
        input,
        env,
        expected: None,
        matched: &[],
        unmatched: &[],
    }
}

const FB: &[&str] = &["FOO=xxx", "BAR="];

#[test]
fn process_with_matches() {
    let cases = [
        ok("x", &["DUMMY=dummy"], "x", &[], &[]),
        ok("x ${UNUSED}", &["DUMMY=dummy"], "x ", &[], &["UNUSED"]),
        ok("x ${FOO}", &["FOO=y"], "x y", &["FOO"], &[]),
        ok(
            "${FOO-aaa} ${BAR-bbb} ${BAZ-ccc}",
            FB,
            "xxx  ccc",
            &["FOO", "BAR"],
            &["BAZ"],
        ),
        ok(
            "${FOO:-aaa} ${BAR:-bbb} ${BAZ:-ccc}",
            FB,
            "xxx bbb ccc",
            &["FOO", "BAR"],
            &["BAZ"],
        ),
        ok("${FOO:-}", FB, "xxx", &["FOO"], &[]),
        ok("${FOO:-}", &[], "", &[], &["FOO"]),
        ok(
            "${FOO+aaa} ${BAR+bbb} ${BAZ+ccc}",
            FB,
            "aaa bbb ",
            &["FOO", "BAR"],
            &["BAZ"],
        ),
        ok(
            "${FOO:+aaa} ${BAR:+bbb} ${BAZ:+ccc}",
            FB,
            "aaa  ",
            &["FOO", "BAR"],
            &["BAZ"],
        ),
        ok("${FOO?aaa}", FB, "xxx", &["FOO"], &[]),
        ok("${BAR?bbb}", FB, "", &["BAR"], &[]),
        err("${BAZ?ccc}", FB),
        ok("${FOO:?aaa}", FB, "xxx", &["FOO"], &[]),
        err("${BAR:?bbb}", FB),
        err("${BAZ:?ccc}", FB),
        err("${BAZ:?}", FB),
        err("${FOO=aaa}", FB),
        err("${FOO=:aaa}", FB),
        err("${FOO=}", &[]),
        ok(
            r"${FOO#()[]{\}^$.\*\?|\\}",
            &[r"FOO=()[]{}^$.*?|\x"],
            "x",
            &["FOO"],
            &[],
        ),
        ok(r"${FOO%%\**}", &["FOO=xx**"], "xx", &["FOO"], &[]),
        ok("${FOO#*x*y}", &["FOO=xxyy"], "y", &["FOO"], &[]),
        ok("${FOO#*}", &["FOO=xxyy"], "xxyy", &["FOO"], &[]),
        ok(
            "${FOO#$BAR}",
            &["FOO=xxyy", "BAR=x"],
            "xyy",
            &["FOO", "BAR"],
            &[],
        ),
        ok(
            "${FOO#$BAR}",
            &["FOO=xxyy", "BAR="],
            "xxyy",
            &["FOO", "BAR"],
            &[],
        ),
        ok("${FOO#}", &["FOO=xxyy"], "xxyy", &["FOO"], &[]),
        ok("${FOO##*x}", &["FOO=xxyy"], "yy", &["FOO"], &[]),
        ok("${FOO##}", &["FOO=xxyy"], "xxyy", &["FOO"], &[]),
        ok(r"${FOO#?\?}", &["FOO=???y"], "?y", &["FOO"], &[]),
        ok(r#"${FOO#"a\b"}"#, &[r"FOO=a\bc"], "c", &["FOO"], &[]),
        ok(r"${FOO#'a\b'}", &[r"FOO=a\bc"], "c", &["FOO"], &[]),
        ok(
            "${ABC:-.}${FOO%x}${ABC:-.}",
            &["FOO=xxyy"],
            ".xxyy.",
            &["FOO"],
            &["ABC"],
        ),
        ok(r"${FOO%%\**\*}", &["FOO=a***yy*"], "a", &["FOO"], &[]),
        ok("${FOO%}", &["FOO=xxyy"], "xxyy", &["FOO"], &[]),
        ok(
            "${FOO%%$BAR}",
            &["FOO=xxyy", "BAR="],
            "xxyy",
            &["FOO", "BAR"],
            &[],
        ),
        err("${FOO:#}", &[]),
        err("${FOO:##}", &[]),
        err("${FOO:%}", &[]),
        err("${FOO:%%}", &[]),
        ok(
            "${FOO/$NEEDLE/.} - ${FOO//$NEEDLE/.}",
            &["FOO=/foo*/*/*.txt", r"NEEDLE=\*/"],
            "/foo.*/*.txt - /foo..*.txt",
            &["FOO", "NEEDLE"],
            &[],
        ),
        ok(
            "${FOO/$NEEDLE/} - ${FOO//$NEEDLE/}",
            &["FOO=/tmp/tmp/bar.txt", "NEEDLE=/tmp"],
            "/tmp/bar.txt - /bar.txt",
            &["FOO", "NEEDLE"],
            &[],
        ),
        ok(
            "${FOO/$NEEDLE/$REPLACEMENT} - ${FOO//$NEEDLE/$REPLACEMENT}",
            &["FOO=/a/foo/b/c.txt", "NEEDLE=/?/", "REPLACEMENT=/"],
            "/foo/b/c.txt - /foo/c.txt",
            &["FOO", "NEEDLE", "REPLACEMENT"],
            &[],
        ),
        ok(
            "${FOO/$NEEDLE/$REPLACEMENT}",
            &[
                "FOO=http://google.de",
                "NEEDLE=http://",
                "REPLACEMENT=https://",
            ],
            "https://google.de",
            &["FOO", "NEEDLE", "REPLACEMENT"],
            &[],
        ),
        ok(
            r"${FOO//\//\/}",
            &["FOO=/tmp/foo.txt"],
            r"\/tmp\/foo.txt",
            &["FOO"],
            &[],
        ),
        ok(
            "${FOO/$BAR/ww}",
            &["FOO=xxyy", "BAR="],
            "wwxxyy",
            &["FOO", "BAR"],
            &[],
        ),
        err("${FOO//ww}", &["FOO=xxyy"]),
        err("${FOO//}", &["FOO=xxyy"]),
        ok("${FOO///}", &["FOO=xxyy"], "xxyy", &["FOO"], &[]),
        ok("${FOO///}", &[], "", &[], &["FOO"]),
    ];
    let lex = Lexer::new('\\');
    for c in cases {
        let got = lex.process_word_with_matches(c.input, &envs(c.env));
        match c.expected {
            None => assert!(got.is_err(), "{:?} gave {got:?}", c.input),
            Some(expected) => {
                let got = got.unwrap_or_else(|e| panic!("{:?}: {e}", c.input));
                assert_eq!(got.result, expected, "{:?}", c.input);
                assert_eq!(got.matched, set(c.matched), "{:?}", c.input);
                assert_eq!(got.unmatched, set(c.unmatched), "{:?}", c.input);
            }
        }
    }
}

#[test]
fn platform_release_name() {
    let lex = Lexer::new('\\');
    let release =
        "something-${VERSION}.${TARGETOS}-${TARGETARCH}${TARGETVARIANT:+-${TARGETVARIANT}}.tar.gz";
    let env = envs(&[
        "VERSION=v1.2.3",
        "TARGETOS=linux",
        "TARGETARCH=arm",
        "TARGETVARIANT=v7",
    ]);
    assert_eq!(
        lex.process_word(release, &env).unwrap(),
        "something-v1.2.3.linux-arm-v7.tar.gz"
    );
    let env = envs(&[
        "VERSION=v1.2.3",
        "TARGETOS=linux",
        "TARGETARCH=arm64",
        "TARGETVARIANT=",
    ]);
    assert_eq!(
        lex.process_word(release, &env).unwrap(),
        "something-v1.2.3.linux-arm64.tar.gz"
    );
    let env = envs(&["VERSION=v1.2.3", "TARGETOS=linux", "TARGETARCH=arm64"]);
    assert_eq!(
        lex.process_word(release, &env).unwrap(),
        "something-v1.2.3.linux-arm64.tar.gz"
    );
}

#[test]
fn skip_unset_env_keeps_references() {
    let lex = Lexer::new('\\').skip_unset_env(true);
    let env = envs(&["A=1"]);
    assert_eq!(
        lex.process_word("$A $B ${B} ${B:-x} ${B/p/r} ${B//p/r}", &env)
            .unwrap(),
        "1 $B ${B} ${B:-x} ${B/p/r} ${B/p/r}"
    );
}

#[test]
fn heredoc_marker_is_one_word() {
    let lex = Lexer::new('\\')
        .raw_quotes(true)
        .raw_escapes(true)
        .skip_unset_env(true);
    assert_eq!(
        lex.process_words("RUN cat << EOF >a <b", &NoEnv).unwrap(),
        vec!["RUN", "cat", "<< EOF", ">a", "<b"]
    );
}

#[test]
fn nested_substitution_resets_the_word_buffer() {
    // BuildKit shares one word buffer between nested calls, so the text
    // before a `${...}` with a modifier is lost from the split words (but
    // not from the single-word result).
    let lex = Lexer::new('\\');
    assert_eq!(lex.process_words("abc${x:-y}", &NoEnv).unwrap(), vec!["y"]);
    assert_eq!(lex.process_word("abc${x:-y}", &NoEnv).unwrap(), "abcy");
    assert_eq!(lex.process_words("abc${x}d", &NoEnv).unwrap(), vec!["abcd"]);
}

#[test]
fn leading_byte_order_mark_is_skipped() {
    let lex = Lexer::new('\\');
    assert_eq!(
        lex.process_word("\u{feff}a\u{feff}", &NoEnv).unwrap(),
        "a\u{feff}"
    );
}

#[test]
fn errors_name_the_word() {
    let e = Lexer::new('\\').process_word("'abc", &NoEnv).unwrap_err();
    assert_eq!(
        e.message(),
        "unexpected end of statement while looking for matching single-quote"
    );
    assert!(
        e.to_string().starts_with("failed to process \"'abc\""),
        "{e}"
    );
    let e = Lexer::new('\\').process_word("${a", &NoEnv).unwrap_err();
    assert_eq!(
        e.message(),
        "unsupported modifier (\u{fffd}) in substitution"
    );
}
