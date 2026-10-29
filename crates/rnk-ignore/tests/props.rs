//! Property tests: no panics on any input, the algebraic properties of
//! `.dockerignore` semantics, and agreement with a reference matcher that
//! interprets simple globs directly instead of through regular expressions.

use proptest::prelude::*;
use rnk_ignore::{MatchInfo, Matcher, read_patterns};

/// Pattern pieces, weighted towards the syntax that needs translating.
fn pattern_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "a",
        "b",
        "/",
        "*",
        "**",
        "?",
        ".",
        "..",
        "[",
        "]",
        "[a-c]",
        "[^a]",
        "\\",
        "\\*",
        "\\d",
        "\\b",
        "\\pL",
        "\\p{Greek}",
        "\\x{D800}",
        "\\Q",
        "\\E",
        "{",
        "}",
        "{2}",
        "|",
        "^",
        "$",
        "(",
        ")",
        "+",
        "!",
        " ",
        "\u{e9}",
        "\u{feff}",
        "[[:alpha:]]",
        "\n",
    ])
}

fn pattern() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(pattern_piece(), 0..8).prop_map(|p| p.concat().into_bytes()),
        prop::collection::vec(any::<u8>(), 0..12),
    ]
}

fn path() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(
            prop::sample::select(vec![
                "a", "b", "/", ".", "..", "*", "\u{e9}", "\u{fffd}", "\n"
            ]),
            0..8
        )
        .prop_map(|p| p.concat().into_bytes()),
        prop::collection::vec(any::<u8>(), 0..12),
    ]
}

/// Calls `matches_using_parent` on each prefix of `path`, as a walk does.
fn along_walk(m: &Matcher, path: &[u8]) -> Result<bool, rnk_ignore::Error> {
    let mut info = MatchInfo::default();
    let mut matched = false;
    let mut end = 0;
    for (i, part) in path.split(|&b| b == b'/').enumerate() {
        end += part.len() + usize::from(i > 0);
        (matched, info) = m.matches_using_parent(&path[..end], &info)?;
    }
    Ok(matched)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Nothing panics, whatever the input.
    #[test]
    fn never_panics(patterns in prop::collection::vec(pattern(), 0..4), file in prop::collection::vec(any::<u8>(), 0..64), p in path()) {
        // Only panics matter here, not the results.
        let _ = read_patterns(&file);
        if let Ok(m) = Matcher::new(&patterns) {
            let _ = m.matches_or_parent_matches(&p);
            let _ = along_walk(&m, &p);
            let _ = m.may_reinclude_below(&p);
        }
    }

    /// An exception right after its own pattern re-includes everything the
    /// pattern excluded.
    #[test]
    fn exception_undoes_pattern(raw in pattern(), p in path()) {
        let Ok(read) = read_patterns(&raw) else { return Ok(()) };
        // `!` glued to `/` or to a leading `..` changes how the exception
        // cleans, and `New` trims `p` but not the `p` in `!p`; then the
        // exception is not the same pattern any more.
        let same_after_bang = |p: &Vec<u8>| {
            let text = String::from_utf8_lossy(p);
            !p.is_empty() && p[0] != b'!' && p != b"/" && !p.starts_with(b"..") && text.trim() == text
        };
        let Some(pat) = read.into_iter().find(same_after_bang) else {
            return Ok(());
        };
        let Ok(only) = Matcher::new(std::slice::from_ref(&pat)) else { return Ok(()) };
        let exception = [b"!".as_slice(), &pat].concat();
        let both = Matcher::new(&[pat.clone(), exception]).unwrap();
        match only.matches_or_parent_matches(&p) {
            Ok(true) => prop_assert_eq!(both.matches_or_parent_matches(&p), Ok(false)),
            Ok(false) => prop_assert_eq!(both.matches_or_parent_matches(&p), Ok(false)),
            Err(_) => prop_assert!(both.matches_or_parent_matches(&p).is_err()),
        }
    }

    /// `./p` and `/p` in a `.dockerignore` are the same pattern as `p`
    /// (for `p` without `..`, which a leading `/` would absorb).
    #[test]
    fn dot_slash_and_slash_prefixes_are_ignored(p in "[ab*?][ab*?.]{0,3}(/{1,2}(\\./)?[ab*?][ab*?.]{0,3}){0,3}/?") {
        let plain = read_patterns(p.as_bytes()).unwrap();
        prop_assert_eq!(&read_patterns(format!("./{p}").as_bytes()).unwrap(), &plain);
        prop_assert_eq!(&read_patterns(format!("/{p}").as_bytes()).unwrap(), &plain);
    }

    /// Without exceptions, the walk and the parent check agree.
    #[test]
    fn walk_agrees_without_exceptions(patterns in prop::collection::vec("[ab*?./]{1,6}", 1..4), p in "[ab][ab.]{0,2}(/[ab][ab.]{0,2}){0,3}") {
        let m = Matcher::new(&patterns).unwrap();
        prop_assert!(!m.has_exclusions());
        prop_assert_eq!(along_walk(&m, p.as_bytes()), m.matches_or_parent_matches(&p));
    }

    /// Simple globs agree with the reference matcher.
    #[test]
    fn simple_globs_agree_with_reference(
        patterns in prop::collection::vec(("!?", "[ab*?./]{1,7}"), 1..4),
        p in "[ab./]{0,9}",
    ) {
        let patterns: Vec<String> = patterns.into_iter().map(|(bang, p)| format!("{bang}{p}")).collect();
        let Ok(m) = Matcher::new(&patterns) else {
            // Only a lone `!` (such as `!/` once cleaned) is rejected.
            prop_assert!(patterns.iter().any(|p| reference::clean(p.as_bytes()) == b"!"));
            return Ok(());
        };
        prop_assert_eq!(m.matches_or_parent_matches(&p), Ok(reference::matches_or_parent_matches(&patterns, p.as_bytes())), "{:?} {:?}", patterns, p);
    }
}

/// A direct interpreter of patternmatcher's semantics for globs made of
/// literals, `/`, `.`, `*`, `?` and `**`, written from the documented
/// rules rather than from the port: Go's path cleaning, the four match
/// types, and a backtracking matcher instead of regular expressions.
mod reference {
    /// `filepath.Clean`, element by element.
    pub(crate) fn clean(path: &[u8]) -> Vec<u8> {
        let rooted = path.first() == Some(&b'/');
        let mut parts: Vec<&[u8]> = Vec::new();
        for part in path.split(|&b| b == b'/') {
            match part {
                b"" | b"." => {}
                b".." => {
                    if parts.last().is_some_and(|p| *p != b"..") {
                        parts.pop();
                    } else if !rooted {
                        parts.push(part);
                    }
                }
                _ => parts.push(part),
            }
        }
        let joined = parts.join(&b'/');
        match (rooted, joined.is_empty()) {
            (true, _) => [b"/".as_slice(), &joined].concat(),
            (false, true) => b".".to_vec(),
            (false, false) => joined,
        }
    }

    fn dir(path: &[u8]) -> Vec<u8> {
        match path.iter().rposition(|&b| b == b'/') {
            Some(i) => clean(&path[..=i]),
            None => b".".to_vec(),
        }
    }

    #[derive(Clone, Copy)]
    enum Token {
        Literal(u8),
        /// `?`: one byte that is not `/` (inputs are ASCII).
        One,
        /// `*`: bytes up to the next `/`.
        Star,
        /// `**` followed by more: any number of directories, maybe none.
        Dirs,
        /// `**` at the end: everything.
        Rest,
    }

    enum Kind {
        Exact,
        Prefix,
        Suffix,
        Glob(Vec<Token>),
    }

    /// The match type: exact until a wildcard; `**` at the end of an
    /// otherwise exact pattern makes a prefix, `**` at the start a suffix
    /// (until a later `*` or `?`); anything else a glob.
    fn kind(p: &[u8]) -> Kind {
        #[derive(Clone, Copy, PartialEq)]
        enum State {
            Exact,
            Prefix,
            Suffix,
            Glob,
        }
        let mut tokens = Vec::new();
        let mut state = State::Exact;
        let mut i = 0;
        while i < p.len() {
            let start = i;
            i += 1;
            match p[start] {
                b'*' if p.get(i) == Some(&b'*') => {
                    i += 1;
                    if p.get(i) == Some(&b'/') {
                        i += 1;
                    }
                    let at_end = i == p.len();
                    tokens.push(if at_end { Token::Rest } else { Token::Dirs });
                    state = if at_end && state == State::Exact {
                        State::Prefix
                    } else {
                        State::Glob
                    };
                    if start == 0 {
                        state = State::Suffix;
                    }
                }
                b'*' => {
                    tokens.push(Token::Star);
                    state = State::Glob;
                }
                b'?' => {
                    tokens.push(Token::One);
                    state = State::Glob;
                }
                c => tokens.push(Token::Literal(c)),
            }
        }
        match state {
            State::Exact => Kind::Exact,
            State::Prefix => Kind::Prefix,
            State::Suffix => Kind::Suffix,
            State::Glob => Kind::Glob(tokens),
        }
    }

    fn glob(tokens: &[Token], s: &[u8]) -> bool {
        let Some((&first, rest)) = tokens.split_first() else {
            return s.is_empty();
        };
        match first {
            Token::Literal(c) => s.first() == Some(&c) && glob(rest, &s[1..]),
            Token::One => s.first().is_some_and(|&c| c != b'/') && glob(rest, &s[1..]),
            Token::Star => {
                let run = s.iter().take_while(|&&c| c != b'/').count();
                (0..=run).any(|k| glob(rest, &s[k..]))
            }
            Token::Dirs => {
                glob(rest, s) || (0..s.len()).any(|k| s[k] == b'/' && glob(rest, &s[k + 1..]))
            }
            Token::Rest => rest.is_empty() || glob(rest, &[]),
        }
    }

    fn matches(pattern: &[u8], path: &[u8]) -> bool {
        match kind(pattern) {
            Kind::Exact => path == pattern,
            Kind::Prefix => path.starts_with(&pattern[..pattern.len() - 2]),
            Kind::Suffix => {
                let suffix = &pattern[2..];
                path.ends_with(suffix) || (suffix.first() == Some(&b'/') && path == &suffix[1..])
            }
            Kind::Glob(tokens) => glob(&tokens, path),
        }
    }

    pub(crate) fn matches_or_parent_matches(patterns: &[String], path: &[u8]) -> bool {
        let parent = dir(path);
        let mut matched = false;
        for p in patterns {
            let cleaned = clean(p.trim().as_bytes());
            let (exclusion, pattern) = match cleaned.split_first() {
                Some((b'!', rest)) => (true, rest.to_vec()),
                _ => (false, cleaned),
            };
            if exclusion != matched {
                continue;
            }
            let mut m = matches(&pattern, path);
            if !m && parent != b"." {
                let mut prefix = Vec::new();
                for (i, part) in parent.split(|&b| b == b'/').enumerate() {
                    if i > 0 {
                        prefix.push(b'/');
                    }
                    prefix.extend_from_slice(part);
                    if matches(&pattern, &prefix) {
                        m = true;
                        break;
                    }
                }
            }
            if m {
                matched = !exclusion;
            }
        }
        matched
    }
}
