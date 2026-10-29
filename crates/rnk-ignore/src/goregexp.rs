// Derived from Go, https://go.googlesource.com/go, src/regexp/syntax/parse.go
// and src/regexp/syntax/perl_groups.go at tag go1.26.9.
// Copyright 2011 The Go Authors. Licensed under the BSD 3-Clause license.
// Modified for ranikust: emits `regex` crate syntax instead of a parse tree.

//! Translation of Go regular expressions into `regex` crate syntax.
//!
//! `patternmatcher` builds a Go regular expression from each pattern and
//! hands it to `regexp.Compile`. Go and the `regex` crate do not read the
//! same syntax the same way (`[[:alpha:]]` versus nested classes, `\d`
//! ASCII versus Unicode, octal escapes, `\Q…\E`, `\pL` tables), and
//! `patternmatcher` passes `|`, `{` and `}` through unescaped, so whatever
//! Go makes of them is the behaviour to keep. This module parses the
//! expression as Go's `regexp/syntax` does with the `Perl` flags that
//! `regexp.Compile` uses, rejects what Go rejects, and writes a `regex`
//! expression that matches the same strings: every literal becomes
//! `\x{…}`, every class an explicit list of ranges.
//!
//! Size and nesting limits are not ported: Go fails with "expression too
//! large" or "nests too deeply" only for lines of several kilobytes, and
//! the `regex` crate has limits of its own.

use crate::golang::BadPattern;
use crate::unicode_tables::{NAMES, TABLES};

/// `unicode.MaxRune`.
const MAX_RUNE: u32 = 0x10_FFFF;

/// A class as sorted, merged inclusive code point ranges, and the input
/// left after it.
type Parsed<'a> = (Vec<(u32, u32)>, &'a str);

/// An expression that matches nothing, for a class or a literal that can
/// only match a surrogate code point, which never occurs in decoded input.
///
/// One known divergence from Go: a surrogate literal (`\x{D800}`) inside
/// the literal prefix of a regexp Go runs one-pass is compared as the bytes
/// of U+FFFD there, and matches them. Porting that needs Go's one-pass
/// analysis; see the crate card.
const NEVER: &str = r"(?-u:\b)(?-u:\B)";

/// Translates the Go regular expression `go` (with the flags of
/// `regexp.Compile`) into an equivalent `regex` crate expression.
pub(crate) fn translate(go: &str) -> Result<String, BadPattern> {
    let mut parser = Parser {
        out: String::with_capacity(go.len() * 4),
        top: None,
        groups: Vec::new(),
        last_repeat: false,
    };
    parser.parse(go)?;
    Ok(parser.out)
}

/// A quantifier as Go's `repeatIsValid` sees it: `max` is the repeat count
/// for `{n,m}`, or `None` for `*`, `+` and `?`.
#[derive(Clone, Copy)]
struct Quantifier {
    min: i64,
    max: Option<i64>,
}

/// The expression on top of Go's parse stack: where it starts in the output
/// and the quantifiers applied to it, innermost first.
struct Top {
    start: usize,
    quantifiers: Vec<Quantifier>,
}

struct Parser {
    out: String,
    /// The top of Go's stack, or `None` when it is empty or holds `(` or `|`
    /// (a quantifier there is "missing argument to repetition operator").
    top: Option<Top>,
    /// Output offsets of the open groups.
    groups: Vec<usize>,
    /// Whether the previous token was a quantifier (Go rejects `a**`).
    last_repeat: bool,
}

impl Parser {
    fn parse(&mut self, mut t: &str) -> Result<(), BadPattern> {
        while let Some(c) = t.chars().next() {
            let mut repeat = false;
            match c {
                '(' => {
                    if t[1..].starts_with('?') {
                        // `compile` only writes `(` as part of `(.*/)?`, so Perl
                        // flag groups never reach this parser.
                        return Err(BadPattern);
                    }
                    self.groups.push(self.out.len());
                    self.out.push_str("(?:");
                    self.top = None;
                    t = &t[1..];
                }
                '|' => {
                    self.out.push('|');
                    self.top = None;
                    t = &t[1..];
                }
                ')' => {
                    let start = self.groups.pop().ok_or(BadPattern)?;
                    self.out.push(')');
                    self.top = Some(Top {
                        start,
                        quantifiers: Vec::new(),
                    });
                    t = &t[1..];
                }
                // With `OneLine`, `^` and `$` match only at the ends of the text,
                // like the `regex` crate's `^` and `$` without the `m` flag.
                '^' => {
                    self.atom("^");
                    t = &t[1..];
                }
                '$' => {
                    self.atom("$");
                    t = &t[1..];
                }
                // Without `DotNL`, `.` matches anything but `\n`, as in `regex`.
                '.' => {
                    self.atom(".");
                    t = &t[1..];
                }
                '[' => {
                    let (class, rest) = parse_class(t)?;
                    self.class(&class);
                    t = rest;
                }
                '*' | '+' | '?' => {
                    let q = match c {
                        '*' => Quantifier { min: 0, max: None },
                        '+' => Quantifier { min: 1, max: None },
                        _ => Quantifier { min: 0, max: None },
                    };
                    let op = &t[..1];
                    t = self.repeat(q, op, &t[1..])?;
                    repeat = true;
                }
                '{' => match parse_repeat(t) {
                    // Not a repetition: a literal `{`.
                    None => {
                        self.literal('{');
                        t = &t[1..];
                    }
                    Some((min, max, after)) => {
                        let op_max = max.unwrap_or(-1);
                        if !(0..=1000).contains(&min)
                            || op_max > 1000
                            || (op_max >= 0 && min > op_max)
                        {
                            return Err(BadPattern);
                        }
                        let op = match max {
                            Some(m) if m == min => format!("{{{min}}}"),
                            Some(m) => format!("{{{min},{m}}}"),
                            None => format!("{{{min},}}"),
                        };
                        t = self.repeat(
                            Quantifier {
                                min,
                                max: Some(op_max),
                            },
                            &op,
                            after,
                        )?;
                        repeat = true;
                    }
                },
                '\\' => t = self.escape(t)?,
                _ => {
                    self.literal(c);
                    t = &t[c.len_utf8()..];
                }
            }
            self.last_repeat = repeat;
        }
        if self.groups.is_empty() {
            Ok(())
        } else {
            Err(BadPattern)
        }
    }

    /// Pushes an operand.
    fn atom(&mut self, text: &str) {
        self.top = Some(Top {
            start: self.out.len(),
            quantifiers: Vec::new(),
        });
        self.out.push_str(text);
    }

    fn literal(&mut self, c: char) {
        self.atom(&format!("\\x{{{:X}}}", u32::from(c)));
    }

    /// A literal given as a code point, which may be a surrogate.
    fn literal_code(&mut self, r: u32) {
        match char::from_u32(r) {
            Some(c) => self.literal(c),
            None => self.atom(NEVER),
        }
    }

    fn class(&mut self, ranges: &[(u32, u32)]) {
        let text = emit_class(ranges);
        self.atom(&text);
    }

    /// Go's `repeat`: applies the quantifier `op` to the top of the stack.
    /// Returns the input after the operator and an optional non-greedy `?`.
    fn repeat<'t>(
        &mut self,
        q: Quantifier,
        op: &str,
        mut after: &'t str,
    ) -> Result<&'t str, BadPattern> {
        // A trailing `?` makes it non-greedy, which cannot change whether a
        // match exists.
        if let Some(rest) = after.strip_prefix('?') {
            after = rest;
        }
        if self.last_repeat {
            return Err(BadPattern);
        }
        let Some(top) = self.top.as_mut() else {
            return Err(BadPattern);
        };
        top.quantifiers.push(q);
        if q.max.is_some()
            && (q.min >= 2 || q.max >= Some(2))
            && !repeat_is_valid(&top.quantifiers, 1000)
        {
            return Err(BadPattern);
        }
        self.out.insert_str(top.start, "(?:");
        self.out.push(')');
        self.out.push_str(op);
        Ok(after)
    }

    /// A `\` outside a class. Returns the input after the escape.
    fn escape<'t>(&mut self, t: &'t str) -> Result<&'t str, BadPattern> {
        match t.as_bytes().get(1) {
            Some(b'A') => {
                self.atom(r"\A");
                return Ok(&t[2..]);
            }
            // Go's word boundaries are ASCII ones.
            Some(b'b') => {
                self.atom(r"(?-u:\b)");
                return Ok(&t[2..]);
            }
            Some(b'B') => {
                self.atom(r"(?-u:\B)");
                return Ok(&t[2..]);
            }
            Some(b'C') => return Err(BadPattern),
            Some(b'Q') => {
                // `\Q…\E`: everything up to `\E`, or to the end, is literal.
                let (lit, rest) = t[2..].split_once(r"\E").unwrap_or((&t[2..], ""));
                for c in lit.chars() {
                    self.literal(c);
                }
                return Ok(rest);
            }
            Some(b'z') => {
                self.atom(r"\z");
                return Ok(&t[2..]);
            }
            _ => {}
        }
        if let Some((ranges, rest)) = parse_unicode_class(t)? {
            self.class(&ranges);
            return Ok(rest);
        }
        if let Some((ranges, rest)) = parse_perl_class(t) {
            self.class(&ranges);
            return Ok(rest);
        }
        let (r, rest) = parse_escape(t)?;
        self.literal_code(r);
        Ok(rest)
    }
}

/// Go's `repeatIsValid` for a chain of quantifiers on one operand (the only
/// nesting `compile`'s output allows): the product of the `{n,m}` counts
/// must stay within `n`.
fn repeat_is_valid(chain: &[Quantifier], mut n: i64) -> bool {
    for q in chain.iter().rev() {
        let Some(max) = q.max else { continue };
        if max == 0 {
            return true;
        }
        let m = if max < 0 { q.min } else { max };
        if m > n {
            return false;
        }
        if m > 0 {
            n /= m;
        }
    }
    true
}

/// Go's `parseRepeat`: `{min}`, `{min,}` or `{min,max}` at the start of
/// `s`. Returns `(min, max, rest)`, `max` being `None` for `{min,}`, or
/// `None` when `s` does not have that form. A count too large to parse is
/// returned as -1, and makes `min` -1.
fn parse_repeat(s: &str) -> Option<(i64, Option<i64>, &str)> {
    let s = s.strip_prefix('{')?;
    let (mut min, mut s) = parse_int(s)?;
    let max;
    if let Some(rest) = s.strip_prefix(',') {
        s = rest;
        if s.is_empty() {
            return None;
        }
        if let Some(rest) = s.strip_prefix('}') {
            return Some((min, None, rest));
        }
        let (m, rest) = parse_int(s)?;
        s = rest;
        if m < 0 {
            min = -1;
        }
        max = Some(m);
    } else {
        if s.is_empty() {
            return None;
        }
        max = Some(min);
    }
    let rest = s.strip_prefix('}')?;
    Some((min, max, rest))
}

/// Go's `parseInt`: a decimal number without leading zeros; -1 when it is
/// too large.
fn parse_int(s: &str) -> Option<(i64, &str)> {
    let b = s.as_bytes();
    if !b.first()?.is_ascii_digit() {
        return None;
    }
    if b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit() {
        return None;
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    let mut n: i64 = 0;
    for &d in &b[..digits] {
        if n >= 100_000_000 {
            n = -1;
            break;
        }
        n = n * 10 + i64::from(d - b'0');
    }
    Some((n, &s[digits..]))
}

/// Go's `parseEscape`: a single-character escape at the start of `s`.
fn parse_escape(s: &str) -> Result<(u32, &str), BadPattern> {
    let mut chars = s[1..].chars();
    let c = chars.next().ok_or(BadPattern)?;
    let mut t = chars.as_str();
    let is_octal = |t: &str| {
        t.as_bytes()
            .first()
            .is_some_and(|b| (b'0'..=b'7').contains(b))
    };
    match c {
        // A single non-zero digit would be a backreference: not supported.
        '1'..='7' if !is_octal(t) => Err(BadPattern),
        '0'..='7' => {
            // Up to three octal digits; one is consumed already.
            let mut r = u32::from(c) - u32::from('0');
            for _ in 1..3 {
                if !is_octal(t) {
                    break;
                }
                r = r * 8 + u32::from(t.as_bytes()[0] - b'0');
                t = &t[1..];
            }
            Ok((r, t))
        }
        'x' => {
            let mut chars = t.chars();
            let c = chars.next().ok_or(BadPattern)?;
            t = chars.as_str();
            if c == '{' {
                // Any number of hex digits in braces, at least one.
                let mut r: u32 = 0;
                let mut nhex = 0;
                loop {
                    let mut chars = t.chars();
                    let c = chars.next().ok_or(BadPattern)?;
                    t = chars.as_str();
                    if c == '}' {
                        break;
                    }
                    let v = c.to_digit(16).ok_or(BadPattern)?;
                    r = r * 16 + v;
                    if r > MAX_RUNE {
                        return Err(BadPattern);
                    }
                    nhex += 1;
                }
                if nhex == 0 {
                    return Err(BadPattern);
                }
                return Ok((r, t));
            }
            // Exactly two hex digits.
            let x = c.to_digit(16);
            let mut chars = t.chars();
            let y = chars.next().and_then(|c| c.to_digit(16));
            t = chars.as_str();
            match (x, y) {
                (Some(x), Some(y)) => Ok((x * 16 + y, t)),
                _ => Err(BadPattern),
            }
        }
        // C escapes; `\b` is a word boundary, never a backspace.
        'a' => Ok((0x07, t)),
        'f' => Ok((0x0C, t)),
        'n' => Ok((0x0A, t)),
        'r' => Ok((0x0D, t)),
        't' => Ok((0x09, t)),
        'v' => Ok((0x0B, t)),
        // Escaped ASCII punctuation (and `_`) is itself.
        c if c.is_ascii() && !c.is_ascii_alphanumeric() => Ok((u32::from(c), t)),
        _ => Err(BadPattern),
    }
}

/// Go's `parseClassChar`: a class member, possibly escaped.
fn parse_class_char(s: &str) -> Result<(u32, &str), BadPattern> {
    match s.chars().next() {
        None => Err(BadPattern),
        Some('\\') => parse_escape(s),
        Some(c) => Ok((u32::from(c), &s[c.len_utf8()..])),
    }
}

// Go's `perlGroup` and `posixGroup` (perl_groups.go).
const DIGIT: &[(u32, u32)] = &[(0x30, 0x39)];
const PERL_SPACE: &[(u32, u32)] = &[(0x9, 0xA), (0xC, 0xD), (0x20, 0x20)];
const WORD: &[(u32, u32)] = &[(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)];

const POSIX_GROUPS: &[(&str, &[(u32, u32)])] = &[
    ("alnum", &[(0x30, 0x39), (0x41, 0x5A), (0x61, 0x7A)]),
    ("alpha", &[(0x41, 0x5A), (0x61, 0x7A)]),
    ("ascii", &[(0x0, 0x7F)]),
    ("blank", &[(0x9, 0x9), (0x20, 0x20)]),
    ("cntrl", &[(0x0, 0x1F), (0x7F, 0x7F)]),
    ("digit", DIGIT),
    ("graph", &[(0x21, 0x7E)]),
    ("lower", &[(0x61, 0x7A)]),
    ("print", &[(0x20, 0x7E)]),
    (
        "punct",
        &[(0x21, 0x2F), (0x3A, 0x40), (0x5B, 0x60), (0x7B, 0x7E)],
    ),
    ("space", &[(0x9, 0xD), (0x20, 0x20)]),
    ("upper", &[(0x41, 0x5A)]),
    ("word", WORD),
    ("xdigit", &[(0x30, 0x39), (0x41, 0x46), (0x61, 0x66)]),
];

/// Go's `parsePerlClassEscape`: `\d`, `\s`, `\w` and their negations.
fn parse_perl_class(s: &str) -> Option<Parsed<'_>> {
    let (group, negated) = match s.as_bytes().get(..2)? {
        br"\d" => (DIGIT, false),
        br"\D" => (DIGIT, true),
        br"\s" => (PERL_SPACE, false),
        br"\S" => (PERL_SPACE, true),
        br"\w" => (WORD, false),
        br"\W" => (WORD, true),
        _ => return None,
    };
    Some((signed(group.to_vec(), negated), &s[2..]))
}

/// Go's `parseNamedClass`: a POSIX class such as `[:alpha:]` at the start
/// of `s`. `Ok(None)` when `s` does not have that form; an unknown name is
/// an error. The closing `:]` is searched for in the rest of the whole
/// expression, as Go does.
fn parse_named_class(s: &str) -> Result<Option<Parsed<'_>>, BadPattern> {
    if !s.starts_with("[:") {
        return Ok(None);
    }
    let Some(i) = s[2..].find(":]") else {
        return Ok(None);
    };
    let name = &s[2..2 + i];
    let rest = &s[2 + i + 2..];
    let (name, negated) = match name.strip_prefix('^') {
        Some(n) => (n, true),
        None => (name, false),
    };
    let group = POSIX_GROUPS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or(BadPattern)?;
    Ok(Some((signed(group.1.to_vec(), negated), rest)))
}

/// Go's `canonicalName`: separators (`_`, `-`, space) removed, the first
/// remaining byte upper case and the others lower case. Go keeps non-ASCII
/// bytes as they are, so such a name can never equal one of the (ASCII)
/// table names: `None` stands for it.
fn canonical_name(name: &str) -> Option<String> {
    if !name.is_ascii() {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars().filter(|c| !matches!(c, '_' | '-' | ' ')) {
        out.push(if out.is_empty() {
            c.to_ascii_uppercase()
        } else {
            c.to_ascii_lowercase()
        });
    }
    Some(out)
}

/// Go's `parseUnicodeClass`: `\pX`, `\p{Name}`, `\PX`, `\P{Name}` at the
/// start of `s`. `Ok(None)` when `s` does not start with `\p` or `\P`.
fn parse_unicode_class(s: &str) -> Result<Option<Parsed<'_>>, BadPattern> {
    let mut negated = match s.as_bytes().get(..2) {
        Some(br"\p") => false,
        Some(br"\P") => true,
        _ => return Ok(None),
    };
    let after = &s[2..];
    let (name, rest) = match after.chars().next() {
        Some('{') => {
            let end = s.find('}').ok_or(BadPattern)?;
            (&s[3..end], &s[end + 1..])
        }
        // A one-letter name. At the end of the input Go reads an empty name.
        Some(c) => (&after[..c.len_utf8()], &after[c.len_utf8()..]),
        None => ("", after),
    };
    let name = match name.strip_prefix('^') {
        Some(n) => {
            negated = !negated;
            n
        }
        None => name,
    };
    let canonical = canonical_name(name).ok_or(BadPattern)?;
    let index = NAMES
        .binary_search_by(|(n, _, _)| (*n).cmp(canonical.as_str()))
        .map_err(|_| BadPattern)?;
    let (_, complement, tables) = NAMES[index];
    let mut ranges = Vec::new();
    for &t in tables {
        ranges.extend_from_slice(TABLES[t].1);
    }
    Ok(Some((signed(ranges, negated != complement), rest)))
}

/// Go's `parseClass`: the class at the start of `s` (which starts with
/// `[`), as sorted, merged ranges, and the rest of the input.
fn parse_class(s: &str) -> Result<Parsed<'_>, BadPattern> {
    let mut t = &s[1..];
    let mut negated = false;
    if let Some(rest) = t.strip_prefix('^') {
        negated = true;
        t = rest;
    }
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    // `]` is a member when it comes first. With the Perl flags, `-` is a
    // member anywhere it cannot be a range.
    let mut first = true;
    while t.is_empty() || !t.starts_with(']') || first {
        first = false;
        if t.len() > 2
            && t.starts_with("[:")
            && let Some((group, rest)) = parse_named_class(t)?
        {
            ranges.extend(group);
            t = rest;
            continue;
        }
        if let Some((group, rest)) = parse_unicode_class(t)? {
            ranges.extend(group);
            t = rest;
            continue;
        }
        if let Some((group, rest)) = parse_perl_class(t) {
            ranges.extend(group);
            t = rest;
            continue;
        }
        // A single character or a range.
        let (lo, rest) = parse_class_char(t)?;
        t = rest;
        let mut hi = lo;
        let b = t.as_bytes();
        if b.len() >= 2 && b[0] == b'-' && b[1] != b']' {
            let (h, rest) = parse_class_char(&t[1..])?;
            t = rest;
            if h < lo {
                return Err(BadPattern);
            }
            hi = h;
        }
        ranges.push((lo, hi));
    }
    t = &t[1..];
    Ok((signed(ranges, negated), t))
}

/// Sorts and merges `ranges`, complementing them over `0..=MAX_RUNE` when
/// `negated`.
fn signed(mut ranges: Vec<(u32, u32)>, negated: bool) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    if !negated {
        return merged;
    }
    let mut out = Vec::with_capacity(merged.len() + 1);
    let mut next = 0u32;
    for (lo, hi) in merged {
        if lo > next {
            out.push((next, lo - 1));
        }
        next = hi.saturating_add(1);
    }
    if next <= MAX_RUNE {
        out.push((next, MAX_RUNE));
    }
    out
}

/// Writes sorted, merged ranges as a `regex` class, leaving out surrogate
/// code points, which neither Go nor `regex` input can contain.
fn emit_class(ranges: &[(u32, u32)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("[");
    let mut any = false;
    for &(lo, hi) in ranges {
        for (lo, hi) in [(lo, hi.min(0xD7FF)), (lo.max(0xE000), hi)] {
            if lo <= hi {
                any = true;
                // Writing to a String cannot fail.
                let _ = if lo == hi {
                    write!(out, "\\x{{{lo:X}}}")
                } else {
                    write!(out, "\\x{{{lo:X}}}-\\x{{{hi:X}}}")
                };
            }
        }
    }
    if !any {
        return NEVER.to_owned();
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(go: &str, text: &str) -> Result<bool, BadPattern> {
        let re = translate(go)?;
        let re = regex::Regex::new(&re).map_err(|_| BadPattern)?;
        Ok(re.is_match(text))
    }

    #[test]
    fn literals_and_escapes() {
        assert_eq!(matches(r"^a\.b$", "a.b"), Ok(true));
        assert_eq!(matches(r"^a\.b$", "axb"), Ok(false));
        assert_eq!(matches(r"^\x41\101\x{263A}$", "AA☺"), Ok(true));
        assert_eq!(matches(r"^a\12b$", "a\nb"), Ok(true));
        assert_eq!(matches(r"^\0$", "\0"), Ok(true));
        assert_eq!(matches(r"^\_\-$", "_-"), Ok(true));
        for bad in [
            r"\1",
            r"\8",
            r"\é",
            r"\C",
            r"\E",
            r"\x{110000}",
            r"\xZ1",
            r"\",
            r"\x{}",
        ] {
            assert_eq!(translate(bad), Err(BadPattern), "{bad}");
        }
    }

    #[test]
    fn quote_runs_to_e_or_end() {
        assert_eq!(matches(r"^\Qa[^/]*$", "a[^/]*$"), Ok(true));
        assert_eq!(matches(r"^\Qa.\Eb$", "a.b"), Ok(true));
        assert_eq!(matches(r"^\Qa.\Eb$", "axb"), Ok(false));
    }

    #[test]
    fn classes_follow_go() {
        // `[` inside a class is a member, not a nested class.
        assert_eq!(matches(r"^[a[^/]*]$", "]"), Ok(true));
        assert_eq!(matches(r"^[a[^/]*]$", "a[/^]"), Ok(true));
        assert_eq!(matches(r"^[a[^/]*]$", "a"), Ok(false));
        assert_eq!(matches(r"^[[:alpha:]]$", "q"), Ok(true));
        assert_eq!(matches(r"^[[:^alpha:]]$", "q"), Ok(false));
        assert_eq!(matches(r"^[[:x]$", ":"), Ok(true));
        assert_eq!(translate(r"^[[:x:]]$"), Err(BadPattern));
        assert_eq!(matches(r"^[]a]$", "]"), Ok(true));
        assert_eq!(matches(r"^[^]a]$", "b"), Ok(true));
        assert_eq!(matches(r"^[a-]$", "-"), Ok(true));
        assert_eq!(matches(r"^[\d]$", "٣"), Ok(false), "\\d is ASCII");
        assert_eq!(matches(r"^\s$", "\x0B"), Ok(false), "\\s has no \\v");
        assert_eq!(
            matches(r"^[&&b]$", "&"),
            Ok(true),
            "no class set operations"
        );
        assert_eq!(matches(r"^[^a]$", "\n"), Ok(true));
        for bad in [r"[z-a]", r"[a", r"[]", r"[^]", r"[\b]", r"[a-\d]", r"[\Q]"] {
            assert_eq!(translate(bad), Err(BadPattern), "{bad}");
        }
    }

    #[test]
    fn unicode_classes_use_go_tables() {
        assert_eq!(matches(r"^\pL$", "é"), Ok(true));
        assert_eq!(
            matches(r"^\pl$", "é"),
            Ok(true),
            "names are case-insensitive"
        );
        assert_eq!(matches(r"^\PL$", "é"), Ok(false));
        assert_eq!(matches(r"^\p{Greek}$", "α"), Ok(true));
        assert_eq!(matches(r"^\p{^Greek}$", "α"), Ok(false));
        assert_eq!(matches(r"^\p{greek}$", "α"), Ok(true));
        assert_eq!(matches(r"^\p{Letter}$", "α"), Ok(true));
        assert_eq!(matches(r"^[\P{Any}]$", "a"), Ok(false));
        assert_eq!(matches(r"^\p{Assigned}$", "\u{378}"), Ok(false));
        for bad in [r"\p{Foo}", r"\p{Old_Italic}", r"\p", r"\pX", r"\p{L"] {
            assert_eq!(translate(bad), Err(BadPattern), "{bad}");
        }
        assert_eq!(crate::unicode_tables::UNICODE_VERSION, "15.0.0");
    }

    #[test]
    fn operators_follow_go() {
        assert_eq!(matches(r"^a|b$", "ax"), Ok(true));
        assert_eq!(matches(r"^a{2}$", "aa"), Ok(true));
        assert_eq!(matches(r"^a{2,}$", "aaa"), Ok(true));
        assert_eq!(matches(r"^a{,2}$", "a{,2}"), Ok(true), "not a repetition");
        assert_eq!(matches(r"^x{$", "x{"), Ok(true));
        assert_eq!(matches(r"^{2}x$", "x"), Ok(true), "repeated anchor");
        assert_eq!(matches(r"^(.*/)?a$", "b/a"), Ok(true));
        assert_eq!(matches(r"^.*$", "a\nb"), Ok(false), ". excludes newline");
        assert_eq!(matches(r"^a\b$", "a"), Ok(true));
        assert_eq!(matches(r"^é\b$", "é"), Ok(false), "ASCII word boundary");
        assert_eq!(matches(r"^a{2}\Q\E{3}$", "aaaaaa"), Ok(true));
        // `{01}` is not a repetition (leading zero), so `{2}` repeats `}`.
        assert_eq!(matches(r"^a{01}{2}$", "a{01}}"), Ok(true));
        for bad in [
            r"a**",
            r"a*{2}",
            r"|*",
            r"(*",
            r"a{1001}",
            r"a{2,1}",
            r"a{100}\Q\E{100}",
            r"a)",
            r"(a",
            r"(?i)a",
        ] {
            assert_eq!(translate(bad), Err(BadPattern), "{bad}");
        }
    }

    #[test]
    fn surrogates_never_match() {
        assert_eq!(matches(r"^\x{D800}?a$", "a"), Ok(true));
        assert_eq!(matches(r"^[\x{D800}-\x{DFFF}]?a$", "a"), Ok(true));
        assert_eq!(matches(r"^[^\x00-\x{10FFFF}]$", "a"), Ok(false));
    }
}
