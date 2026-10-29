//! Property tests: no input makes the parser, the instruction layer, the
//! expansion or the lexer panic, and every error about the text carries its
//! lines.

use std::collections::BTreeMap;

use proptest::prelude::*;
use rnk_dockerfile::{Lexer, Lines, parse, parse_ast};

/// Characters that matter to the parser and the lexer, plus a few others.
const ALPHABET: &[&str] = &[
    "a", "Z", "0", "9", " ", "\t", "\n", "\r", "\\", "`", "$", "{", "}", "(", ")", ":", "-", "+",
    "?", "#", "%", "/", "*", "'", "\"", "<", "<<", "=", ",", "[", "]", "é", "\u{a0}", "\u{feff}",
    "\u{85}", "한", "😀",
];

/// Whole tokens that make inputs look like Dockerfiles.
const TOKENS: &[&str] = &[
    "FROM ",
    "RUN ",
    "COPY ",
    "ADD ",
    "ENV ",
    "ARG ",
    "LABEL ",
    "WORKDIR ",
    "USER ",
    "EXPOSE ",
    "VOLUME ",
    "CMD ",
    "ENTRYPOINT ",
    "SHELL ",
    "STOPSIGNAL ",
    "HEALTHCHECK ",
    "ONBUILD ",
    "MAINTAINER ",
    "AS ",
    "x ",
    "--from=",
    "--mount=",
    "type=cache,",
    "target=/a,",
    "--network=",
    "--chown=",
    "--interval=",
    "CMD ",
    "NONE",
    "<<EOF",
    "<<-'E'",
    "EOF\n",
    "E\n",
    "${A:-b}",
    "${A#*/}",
    "${A//x/y}",
    "$A",
    "[\"a\", \"b\"]",
    "[1]",
    "# escape=`\n",
    "# syntax=x\n",
    "# check=y\n",
    "\\\n",
    "`\n",
    "\n",
    " \\ \n",
    "# c\n",
    "\"q",
    "'q'",
];

fn text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        prop::sample::select(ALPHABET).prop_map(str::to_owned),
        prop::sample::select(TOKENS).prop_map(str::to_owned),
        any::<char>().prop_map(String::from),
    ];
    prop::collection::vec(piece, 0..60).prop_map(|v| v.concat())
}

fn env() -> BTreeMap<String, String> {
    [("A", "a/b/c"), ("B", ""), ("C", "x y"), ("PATH", "/bin")]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn check_lines(lines: Option<Lines>, src: &str) {
    let l = lines.expect("an error about the text carries its lines");
    let n = src.split('\n').count().max(1);
    assert!(
        l.start >= 1 && l.start <= l.end && l.end <= n + 1,
        "lines {l:?} out of range for {n} lines"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn parse_never_panics(src in text()) {
        match parse_ast(&src) {
            Ok(ast) => {
                for n in &ast.nodes {
                    prop_assert!(n.lines.start >= 1 && n.lines.start <= n.lines.end);
                }
            }
            Err(e) => check_lines(e.lines(), &src),
        }
        match parse(&src) {
            Ok(df) => {
                let lex = Lexer::new(df.escape);
                let env = env();
                for stage in df.stages {
                    for mut i in stage.instructions {
                        if let Err(e) = i.expand(&lex, &env) {
                            check_lines(e.lines(), &src);
                        }
                    }
                }
            }
            Err(e) => check_lines(e.lines(), &src),
        }
    }

    #[test]
    fn parse_arbitrary_unicode(src in "\\PC{0,200}") {
        if let Err(e) = parse(&src) {
            check_lines(e.lines(), &src);
        }
    }

    #[test]
    fn lexer_never_panics(
        word in text(),
        backtick in any::<bool>(),
        raw_quotes in any::<bool>(),
        raw_escapes in any::<bool>(),
        skip_unset in any::<bool>(),
        skip_quotes in any::<bool>(),
    ) {
        let lex = Lexer::new(if backtick { '`' } else { '\\' })
            .raw_quotes(raw_quotes)
            .raw_escapes(raw_escapes)
            .skip_unset_env(skip_unset)
            .skip_process_quotes(skip_quotes);
        let env = env();
        let one = lex.process_word(&word, &env);
        let many = lex.process_words(&word, &env);
        // Both modes run the same scanner: they fail on the same input.
        prop_assert_eq!(one.is_ok(), many.is_ok());
    }
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let lex = Lexer::new('\\');
    let deep = format!("{}x{}", "${A:-".repeat(5000), "}".repeat(5000));
    assert!(lex.process_word(&deep, &env()).is_err());
    let src = format!(
        "FROM a\nRUN {}x{}\n",
        "${A:-".repeat(5000),
        "}".repeat(5000)
    );
    // The RUN command is not expanded, so this parses; heredoc detection
    // lexes the line and gives up quietly.
    assert!(parse(&src).is_ok());
    let src = format!(
        "FROM a\nCMD {}\"a\"{}\n",
        "[".repeat(20_000),
        "]".repeat(20_000)
    );
    assert!(parse(&src).is_ok());
}
