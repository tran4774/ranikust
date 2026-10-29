// Derived from Go, https://go.googlesource.com/go, src/path/filepath/match.go,
// src/internal/filepathlite/path.go, src/strings/strings.go and
// src/unicode/utf8/utf8.go at tag go1.26.9.
// Copyright 2009-2024 The Go Authors. Licensed under the BSD 3-Clause license.
// Modified for ranikust: byte strings, `/` as the only separator.

//! The parts of Go's standard library whose exact behaviour `patternmatcher`
//! depends on: `filepath.Clean`, `filepath.Dir`, `filepath.Match` (for its
//! error state), `strings.TrimSpace` and UTF-8 decoding.
//!
//! Go strings are byte strings that may hold invalid UTF-8, so everything
//! here works on bytes. Where Go decodes runes, an invalid byte decodes to
//! U+FFFD and consumes exactly one byte, as `utf8.DecodeRune` does.

use std::borrow::Cow;

/// `utf8.RuneError`, what Go decodes an invalid byte to.
pub(crate) const RUNE_ERROR: char = char::REPLACEMENT_CHARACTER;

/// `filepath.ErrBadPattern`: the pattern is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadPattern;

/// `utf8.DecodeRune`: the first rune of `s` and its width in bytes.
///
/// Returns `(RUNE_ERROR, 0)` for an empty slice and `(RUNE_ERROR, 1)` when
/// `s` does not start with a valid UTF-8 sequence.
pub(crate) fn decode_rune(s: &[u8]) -> (char, usize) {
    let Some(&first) = s.first() else {
        return (RUNE_ERROR, 0);
    };
    if first.is_ascii() {
        return (char::from(first), 1);
    }
    let width = match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return (RUNE_ERROR, 1),
    };
    // `str::from_utf8` rejects overlong forms, surrogates and values above
    // U+10FFFF, exactly like Go's decoder.
    match s.get(..width).map(std::str::from_utf8) {
        Some(Ok(text)) => match text.chars().next() {
            Some(c) => (c, width),
            None => (RUNE_ERROR, 1),
        },
        _ => (RUNE_ERROR, 1),
    }
}

/// `utf8.DecodeLastRune`: the last rune of `s` and its width in bytes.
fn decode_last_rune(s: &[u8]) -> (char, usize) {
    let Some(&last) = s.last() else {
        return (RUNE_ERROR, 0);
    };
    if last.is_ascii() {
        return (char::from(last), 1);
    }
    // A valid final rune is the shortest suffix that is valid UTF-8: Go
    // backs up to the nearest rune-start byte and decodes from there.
    for width in 2..=4.min(s.len()) {
        if let Ok(text) = std::str::from_utf8(&s[s.len() - width..])
            && let Some(c) = text.chars().next()
        {
            return (c, width);
        }
    }
    (RUNE_ERROR, 1)
}

/// Decodes `s` into runes the way Go does: each invalid byte becomes one
/// U+FFFD. Borrows when `s` is valid UTF-8.
pub(crate) fn go_runes(s: &[u8]) -> Cow<'_, str> {
    if let Ok(text) = std::str::from_utf8(s) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut rest = s;
    while !rest.is_empty() {
        let (c, width) = decode_rune(rest);
        out.push(c);
        rest = &rest[width..];
    }
    Cow::Owned(out)
}

/// `unicode.IsSpace`. Rust's `char::is_whitespace` uses the same Unicode
/// `White_Space` property, which also covers Go's Latin-1 special cases.
fn is_space(c: char) -> bool {
    c.is_whitespace()
}

/// `strings.TrimSpace`: removes leading and trailing Unicode white space.
/// Trimming stops at an invalid byte, which decodes to U+FFFD.
pub(crate) fn trim_space(mut s: &[u8]) -> &[u8] {
    loop {
        let (c, width) = decode_rune(s);
        if width == 0 || !is_space(c) {
            break;
        }
        s = &s[width..];
    }
    loop {
        let (c, width) = decode_last_rune(s);
        if width == 0 || !is_space(c) {
            break;
        }
        s = &s[..s.len() - width];
    }
    s
}

/// `filepath.Clean` on Unix: the shortest path equivalent to `path` by
/// purely lexical processing.
pub(crate) fn clean(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let rooted = path[0] == b'/';
    let n = path.len();
    // `dotdot` is the index in `out` where `..` must stop, either because it
    // is the leading slash or a leading `../../..` prefix.
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let (mut r, mut dotdot) = (0, 0);
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }
    while r < n {
        let at = |i: usize| path.get(i).copied();
        if path[r] == b'/' {
            // Empty path element.
            r += 1;
        } else if path[r] == b'.' && matches!(at(r + 1), None | Some(b'/')) {
            // `.` element.
            r += 1;
        } else if path[r] == b'.'
            && at(r + 1) == Some(b'.')
            && matches!(at(r + 2), None | Some(b'/'))
        {
            // `..` element: remove to the last separator.
            r += 2;
            if out.len() > dotdot {
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // Cannot backtrack, but not rooted, so append a `..` element.
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            // A real path element: add a slash if needed, then copy it.
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && path[r] != b'/' {
                out.push(path[r]);
                r += 1;
            }
        }
    }
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `filepath.Dir` on Unix: everything but the last element, cleaned.
pub(crate) fn dir(path: &[u8]) -> Vec<u8> {
    let end = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    clean(&path[..end])
}

/// `filepath.Match` on Unix, including its quirk of validating the pattern
/// only as far as matching gets: a malformed part after the first chunk
/// that fails to match is not reported.
pub(crate) fn match_path(mut pattern: &[u8], mut name: &[u8]) -> Result<bool, BadPattern> {
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // A trailing `*` matches the rest of the name unless it has a `/`.
            return Ok(!name.contains(&b'/'));
        }
        // Look for a match at the current position.
        let (t, ok) = match_chunk(chunk, name)?;
        // If this is the last chunk, the name must be exhausted; otherwise a
        // star could still match.
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue;
        }
        if star {
            // Look for a match skipping i+1 bytes; a star cannot skip `/`.
            let mut i = 0;
            while i < name.len() && name[i] != b'/' {
                let (t, ok) = match_chunk(chunk, &name[i + 1..])?;
                if ok {
                    if pattern.is_empty() && !t.is_empty() {
                        i += 1;
                        continue;
                    }
                    name = t;
                    continue 'pattern;
                }
                i += 1;
            }
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

/// `scanChunk`: the next segment of `pattern`, a non-star string possibly
/// preceded by stars. Returns `(star, chunk, rest)`.
fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while let [b'*', rest @ ..] = pattern {
        pattern = rest;
        star = true;
    }
    let mut in_range = false;
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            // The error check is left to `match_chunk`.
            b'\\' if i + 1 < pattern.len() => i += 1,
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => return (star, &pattern[..i], &pattern[i..]),
            _ => {}
        }
        i += 1;
    }
    (star, pattern, &[])
}

/// `matchChunk`: whether `chunk` matches the beginning of `s`, and the rest
/// of `s` after the match. After a mismatch the chunk is still parsed to the
/// end, to report malformed classes and escapes.
fn match_chunk<'s>(mut chunk: &[u8], mut s: &'s [u8]) -> Result<(&'s [u8], bool), BadPattern> {
    let mut failed = false;
    while let Some(&head) = chunk.first() {
        failed = failed || s.is_empty();
        match head {
            b'[' => {
                // A character class. Runes compare as code point values,
                // with U+FFFD for an invalid byte.
                let mut r = 0u32;
                if !failed {
                    let (c, width) = decode_rune(s);
                    r = u32::from(c);
                    s = &s[width..];
                }
                chunk = &chunk[1..];
                let mut negated = false;
                if chunk.first() == Some(&b'^') {
                    negated = true;
                    chunk = &chunk[1..];
                }
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if chunk.first() == Some(&b']') && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (lo, rest) = get_esc(chunk)?;
                    chunk = rest;
                    let mut hi = lo;
                    // `get_esc` guarantees a non-empty remainder.
                    if chunk.first() == Some(&b'-') {
                        let (h, rest) = get_esc(&chunk[1..])?;
                        hi = h;
                        chunk = rest;
                    }
                    matched = matched || (lo <= r && r <= hi);
                    nrange += 1;
                }
                failed = failed || matched == negated;
            }
            b'?' => {
                if !failed {
                    failed = s[0] == b'/';
                    let (_, width) = decode_rune(s);
                    s = &s[width..];
                }
                chunk = &chunk[1..];
            }
            _ => {
                if head == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return Err(BadPattern);
                    }
                }
                if !failed {
                    failed = chunk[0] != s[0];
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        Ok((&[], false))
    } else {
        Ok((s, true))
    }
}

/// `getEsc`: a possibly escaped character of a class, as a code point.
fn get_esc(mut chunk: &[u8]) -> Result<(u32, &[u8]), BadPattern> {
    match chunk.first() {
        None | Some(b'-' | b']') => return Err(BadPattern),
        Some(b'\\') => {
            chunk = &chunk[1..];
            if chunk.is_empty() {
                return Err(BadPattern);
            }
        }
        Some(_) => {}
    }
    let (c, width) = decode_rune(chunk);
    let rest = &chunk[width..];
    if (c == RUNE_ERROR && width == 1) || rest.is_empty() {
        return Err(BadPattern);
    }
    Ok((u32::from(c), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tables below are Go's `cleantests`, `dirtests` and `matchTests`
    // (src/path/filepath/path_test.go and match_test.go, go1.26.9).

    #[test]
    fn clean_matches_go() {
        let cases: &[(&str, &str)] = &[
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("", "."),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("a/b/c/", "a/b/c"),
            ("./", "."),
            ("../", ".."),
            ("../../", "../.."),
            ("/abc/", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("abc//", "abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            ("/../abc", "/abc"),
            ("a/../b:/../../c", "../c"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ];
        for (input, want) in cases {
            assert_eq!(clean(input.as_bytes()), want.as_bytes(), "clean({input:?})");
            // Clean is idempotent.
            assert_eq!(clean(want.as_bytes()), want.as_bytes(), "clean({want:?})");
        }
    }

    #[test]
    fn dir_matches_go() {
        let cases: &[(&str, &str)] = &[
            ("", "."),
            (".", "."),
            ("/.", "/"),
            ("/", "/"),
            ("/foo", "/"),
            ("x/", "x"),
            ("abc", "."),
            ("abc/def", "abc"),
            ("a/b/.x", "a/b"),
            ("a/b/c.", "a/b"),
            ("a/b/c.x", "a/b"),
        ];
        for (input, want) in cases {
            assert_eq!(dir(input.as_bytes()), want.as_bytes(), "dir({input:?})");
        }
    }

    #[test]
    fn match_matches_go() {
        let cases: &[(&str, &str, Result<bool, BadPattern>)] = &[
            ("abc", "abc", Ok(true)),
            ("*", "abc", Ok(true)),
            ("*c", "abc", Ok(true)),
            ("a*", "a", Ok(true)),
            ("a*", "abc", Ok(true)),
            ("a*", "ab/c", Ok(false)),
            ("a*/b", "abc/b", Ok(true)),
            ("a*/b", "a/c/b", Ok(false)),
            ("a*b*c*d*e*/f", "axbxcxdxe/f", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxexxx/f", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxe/xxx/f", Ok(false)),
            ("a*b*c*d*e*/f", "axbxcxdxexxx/fff", Ok(false)),
            ("a*b?c*x", "abxbbxdbxebxczzx", Ok(true)),
            ("a*b?c*x", "abxbbxdbxebxczzy", Ok(false)),
            ("ab[c]", "abc", Ok(true)),
            ("ab[b-d]", "abc", Ok(true)),
            ("ab[e-g]", "abc", Ok(false)),
            ("ab[^c]", "abc", Ok(false)),
            ("ab[^b-d]", "abc", Ok(false)),
            ("ab[^e-g]", "abc", Ok(true)),
            ("a\\*b", "a*b", Ok(true)),
            ("a\\*b", "ab", Ok(false)),
            ("a?b", "a☺b", Ok(true)),
            ("a[^a]b", "a☺b", Ok(true)),
            ("a???b", "a☺b", Ok(false)),
            ("a[^a][^a][^a]b", "a☺b", Ok(false)),
            ("[a-ζ]*", "α", Ok(true)),
            ("*[a-ζ]", "A", Ok(false)),
            ("a?b", "a/b", Ok(false)),
            ("a*b", "a/b", Ok(false)),
            ("[\\]a]", "]", Ok(true)),
            ("[\\-]", "-", Ok(true)),
            ("[x\\-]", "x", Ok(true)),
            ("[x\\-]", "-", Ok(true)),
            ("[x\\-]", "z", Ok(false)),
            ("[\\-x]", "x", Ok(true)),
            ("[\\-x]", "-", Ok(true)),
            ("[\\-x]", "a", Ok(false)),
            ("[]a]", "]", Err(BadPattern)),
            ("[-]", "-", Err(BadPattern)),
            ("[x-]", "x", Err(BadPattern)),
            ("[x-]", "-", Err(BadPattern)),
            ("[x-]", "z", Err(BadPattern)),
            ("[-x]", "x", Err(BadPattern)),
            ("[-x]", "-", Err(BadPattern)),
            ("[-x]", "a", Err(BadPattern)),
            ("\\", "a", Err(BadPattern)),
            ("[a-b-c]", "a", Err(BadPattern)),
            ("[", "a", Err(BadPattern)),
            ("[^", "a", Err(BadPattern)),
            ("[^bc", "a", Err(BadPattern)),
            ("a[", "a", Err(BadPattern)),
            ("a[", "ab", Err(BadPattern)),
            ("a[", "x", Err(BadPattern)),
            ("a/b[", "x", Err(BadPattern)),
            ("*x", "xxx", Ok(true)),
        ];
        for (pattern, name, want) in cases {
            assert_eq!(
                match_path(pattern.as_bytes(), name.as_bytes()),
                *want,
                "Match({pattern:?}, {name:?})"
            );
        }
    }

    #[test]
    fn match_validates_only_as_far_as_it_matches() {
        // The `[` after the failing `a` chunk is never parsed.
        assert_eq!(match_path(b"a*[", b"."), Ok(false));
        assert_eq!(match_path(b"[", b"."), Err(BadPattern));
    }

    #[test]
    fn trim_space_matches_go() {
        assert_eq!(trim_space(b" \t a b \r\n"), b"a b");
        assert_eq!(trim_space("\u{a0}\u{3000}x\u{85}".as_bytes()), b"x");
        // An invalid byte stops trimming: Go decodes it to U+FFFD.
        assert_eq!(trim_space(b" \x80 "), b"\x80");
        // U+FEFF is not white space.
        assert_eq!(trim_space("\u{feff} x".as_bytes()), "\u{feff} x".as_bytes());
        assert_eq!(trim_space(b"   "), b"");
    }

    #[test]
    fn decoding_matches_go() {
        assert_eq!(decode_rune(b""), (RUNE_ERROR, 0));
        assert_eq!(decode_rune("☺x".as_bytes()), ('☺', 3));
        assert_eq!(decode_rune(b"\xe2\x98"), (RUNE_ERROR, 1));
        assert_eq!(decode_rune(b"\xed\xa0\x80"), (RUNE_ERROR, 1), "surrogate");
        assert_eq!(decode_rune(b"\xc0\x80"), (RUNE_ERROR, 1), "overlong");
        assert_eq!(decode_last_rune("x☺".as_bytes()), ('☺', 3));
        assert_eq!(decode_last_rune(b"x\x98"), (RUNE_ERROR, 1));
        // Each byte of a truncated sequence is its own U+FFFD.
        assert_eq!(go_runes(b"a\xe2\x98b"), "a\u{fffd}\u{fffd}b");
    }
}
