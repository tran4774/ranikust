// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/shell/lex.go at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: the shell pattern is compiled into a small Pike VM
// with Go's leftmost-first semantics instead of a Go regular expression.

//! Shell patterns for `${var#word}`, `${var%word}` and `${var/pattern/repl}`.
//!
//! BuildKit converts the pattern into a Go regular expression (`*` becomes
//! `.*` or `.*?`, `?` becomes `.`, everything else is literal) and runs Go's
//! `regexp`. This module compiles the same pattern into a tiny program and
//! runs it with a Pike VM that has the same leftmost-first semantics, so the
//! results match without a regex dependency. As in Go, `.` (and therefore
//! `*` and `?`) never matches a newline.

/// One element of a converted shell pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Lit(char),
    Any,
    Star,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    prog: Vec<Inst>,
    anchored: bool,
}

#[derive(Debug, Clone, Copy)]
enum Inst {
    Char(char),
    Any,
    /// Try the first target, then the second.
    Split(usize, usize),
    Jmp(usize),
    Match,
}

impl Pattern {
    /// BuildKit's `convertShellPatternToRegex`: `greedy` selects `.*` over
    /// `.*?` for `*`, `anchored` matches only at the start of the text.
    pub(crate) fn compile(pattern: &str, greedy: bool, anchored: bool) -> Result<Self, String> {
        let toks = tokenize(pattern)?;
        let mut prog = Vec::with_capacity(toks.len() * 3 + 1);
        for tok in toks {
            match tok {
                Tok::Lit(c) => prog.push(Inst::Char(c)),
                Tok::Any => prog.push(Inst::Any),
                Tok::Star => {
                    let l = prog.len();
                    let (first, second) = if greedy {
                        (l + 1, l + 3)
                    } else {
                        (l + 3, l + 1)
                    };
                    prog.push(Inst::Split(first, second));
                    prog.push(Inst::Any);
                    prog.push(Inst::Jmp(l));
                }
            }
        }
        prog.push(Inst::Match);
        Ok(Self { prog, anchored })
    }

    /// The leftmost-first match at or after `from`, as `(start, end)` char
    /// indices: Go's `FindStringIndex` and the search step of
    /// `ReplaceAllString`.
    pub(crate) fn find_at(&self, text: &[char], from: usize) -> Option<(usize, usize)> {
        let n = self.prog.len();
        let mut clist = Threads::new(n);
        let mut nlist = Threads::new(n);
        let mut matched = None;
        let mut pos = from;
        loop {
            // A new thread starting here has the lowest priority; none is
            // started once a match is known (leftmost wins).
            if matched.is_none() && (!self.anchored || pos == 0) {
                self.add(&mut clist, 0, pos);
            }
            if clist.list.is_empty() {
                break;
            }
            let c = text.get(pos).copied();
            nlist.clear();
            for &(pc, start) in &clist.list {
                match self.prog[pc] {
                    Inst::Match => {
                        matched = Some((start, pos));
                        // Leftmost-first: drop the lower-priority threads.
                        break;
                    }
                    Inst::Char(x) => {
                        if c == Some(x) {
                            self.add(&mut nlist, pc + 1, start);
                        }
                    }
                    Inst::Any => {
                        if c.is_some_and(|c| c != '\n') {
                            self.add(&mut nlist, pc + 1, start);
                        }
                    }
                    Inst::Split(..) | Inst::Jmp(_) => {}
                }
            }
            std::mem::swap(&mut clist, &mut nlist);
            if pos >= text.len() {
                break;
            }
            pos += 1;
        }
        matched
    }

    /// Adds the thread `pc` and everything reachable from it without
    /// consuming input, in priority order.
    fn add(&self, list: &mut Threads, pc: usize, start: usize) {
        let mut stack = vec![pc];
        while let Some(pc) = stack.pop() {
            if list.seen[pc] == list.generation {
                continue;
            }
            list.seen[pc] = list.generation;
            match self.prog[pc] {
                Inst::Split(a, b) => {
                    stack.push(b);
                    stack.push(a);
                }
                Inst::Jmp(t) => stack.push(t),
                Inst::Char(_) | Inst::Any | Inst::Match => list.list.push((pc, start)),
            }
        }
    }
}

struct Threads {
    list: Vec<(usize, usize)>,
    seen: Vec<u64>,
    generation: u64,
}

impl Threads {
    fn new(n: usize) -> Self {
        Self {
            list: Vec::new(),
            seen: vec![0; n],
            generation: 1,
        }
    }

    fn clear(&mut self) {
        self.list.clear();
        self.generation += 1;
    }
}

/// The conversion loop of `convertShellPatternToRegex`. The escape is always
/// `\`, whatever the Dockerfile's escape character: `\*`, `\?` and `\\` are
/// literals, `\}` and `\/` drop the backslash, any other escape is an error.
fn tokenize(pattern: &str) -> Result<Vec<Tok>, String> {
    let mut chars = pattern.chars().peekable();
    // Go's text/scanner skips a byte order mark at the very start.
    if chars.peek() == Some(&'\u{FEFF}') {
        chars.next();
    }
    let mut toks = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '*' => toks.push(Tok::Star),
            '?' => toks.push(Tok::Any),
            '\\' => {
                if matches!(chars.peek(), Some('}' | '/')) {
                    continue;
                }
                match chars.next() {
                    Some(t @ ('*' | '?' | '\\')) => toks.push(Tok::Lit(t)),
                    Some(t) => return Err(format!("invalid escape '\\{t}'")),
                    None => return Err("invalid escape '\\\u{FFFD}'".to_owned()),
                }
            }
            c => toks.push(Tok::Lit(c)),
        }
    }
    Ok(toks)
}

/// `trimPrefix`: removes the shortest (or, if `greedy`, the longest) prefix
/// of `value` that matches `pattern`.
pub(crate) fn trim_prefix(pattern: &str, value: &str, greedy: bool) -> Result<String, String> {
    let re = Pattern::compile(pattern, greedy, true)
        .map_err(|e| format!("invalid pattern ({pattern}) in substitution: {e}"))?;
    let text: Vec<char> = value.chars().collect();
    Ok(match re.find_at(&text, 0) {
        Some((_, end)) => text[end..].iter().collect(),
        None => value.to_owned(),
    })
}

/// `trimSuffix`: reverses the pattern and the value and trims a prefix.
pub(crate) fn trim_suffix(pattern: &str, value: &str, greedy: bool) -> Result<String, String> {
    let pattern = reverse_pattern(pattern);
    let reversed: String = value.chars().rev().collect();
    let trimmed = trim_prefix(&pattern, &reversed, greedy)?;
    Ok(trimmed.chars().rev().collect())
}

/// Reverses a pattern but keeps each `\x` escape in order (`a\*c` → `c\*a`).
fn reverse_pattern(pattern: &str) -> String {
    let p: Vec<char> = pattern.chars().collect();
    let Some(last) = p.len().checked_sub(1) else {
        return String::new();
    };
    let mut out = vec!['\0'; p.len()];
    let mut i = 0;
    while i <= last {
        let out_idx = last - i;
        if p[i] == '\\' && i != last {
            out[out_idx - 1] = '\\';
            out[out_idx] = p[i + 1];
            i += 2;
        } else {
            out[out_idx] = p[i];
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// `${var/pattern/replacement}`: replaces the first match, literally.
pub(crate) fn replace_first(re: &Pattern, value: &str, replacement: &str) -> String {
    let text: Vec<char> = value.chars().collect();
    match re.find_at(&text, 0) {
        Some((s, e)) => {
            let mut out: String = text[..s].iter().collect();
            out.push_str(replacement);
            out.extend(&text[e..]);
            out
        }
        None => value.to_owned(),
    }
}

/// `${var//pattern/replacement}`: Go's `Regexp.ReplaceAllString`, including
/// its handling of empty matches and its `$` expansion in the replacement
/// (`$0` and `${0}` are the match, `$$` is `$`, other references are empty).
pub(crate) fn replace_all(re: &Pattern, value: &str, replacement: &str) -> String {
    let text: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut last_match_end = 0;
    let mut search_pos = 0;
    while search_pos <= text.len() {
        let Some((a0, a1)) = re.find_at(&text, search_pos) else {
            break;
        };
        out.extend(&text[last_match_end..a0]);
        if a1 > last_match_end || a0 == 0 {
            expand_template(&mut out, replacement, &text[a0..a1]);
        }
        last_match_end = a1;
        let width = usize::from(search_pos < text.len());
        if search_pos + width > a1 {
            search_pos += width;
        } else if search_pos + 1 > a1 {
            search_pos += 1;
        } else {
            search_pos = a1;
        }
    }
    out.extend(&text[last_match_end..]);
    out
}

/// Go's `Regexp.expand` for a regular expression without capture groups.
fn expand_template(out: &mut String, template: &str, whole: &[char]) {
    let mut t = template;
    while let Some(i) = t.find('$') {
        out.push_str(&t[..i]);
        t = &t[i + 1..];
        if let Some(rest) = t.strip_prefix('$') {
            out.push('$');
            t = rest;
            continue;
        }
        match extract_ref(t) {
            Some((name, rest)) => {
                t = rest;
                if group_number(name) == Some(0) {
                    out.extend(whole);
                }
            }
            None => out.push('$'),
        }
    }
    out.push_str(t);
}

/// Go's `extract`: a reference name after `$`, optionally in braces, and
/// the rest of the template.
fn extract_ref(s: &str) -> Option<(&str, &str)> {
    let (brace, body) = match s.strip_prefix('{') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let len = body
        .char_indices()
        .find(|&(_, c)| !crate::goutil::is_letter(c) && !crate::goutil::is_digit(c) && c != '_')
        .map_or(body.len(), |(i, _)| i);
    if len == 0 {
        return None;
    }
    let name = &body[..len];
    let mut rest = &body[len..];
    if brace {
        rest = rest.strip_prefix('}')?;
    }
    Some((name, rest))
}

/// Go's group number of a reference name: all ASCII digits, no leading
/// zero, below 10^8.
fn group_number(name: &str) -> Option<u32> {
    if !name.bytes().all(|b| b.is_ascii_digit()) || (name.len() > 1 && name.starts_with('0')) {
        return None;
    }
    name.parse::<u32>().ok().filter(|&n| n < 100_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, greedy: bool, anchored: bool, text: &str) -> Option<(usize, usize)> {
        let t: Vec<char> = text.chars().collect();
        Pattern::compile(pattern, greedy, anchored)
            .unwrap()
            .find_at(&t, 0)
    }

    #[test]
    fn leftmost_first() {
        assert_eq!(find("a*", true, false, "xxabab"), Some((2, 6)));
        assert_eq!(find("a*", false, false, "xxabab"), Some((2, 3)));
        assert_eq!(find("a*b", false, true, "xxabab"), None);
        assert_eq!(find("*b", false, true, "abab"), Some((0, 2)));
        assert_eq!(find("*b", true, true, "abab"), Some((0, 4)));
        assert_eq!(find("", true, false, "abc"), Some((0, 0)));
        assert_eq!(find("?", true, false, "\nx"), Some((1, 2)));
        assert_eq!(find("*", true, true, "ab\ncd"), Some((0, 2)));
        assert_eq!(find("c", true, false, "abc"), Some((2, 3)));
        assert_eq!(find("d", true, false, "abc"), None);
    }

    #[test]
    fn tokenize_follows_convert_shell_pattern_to_regex() {
        assert_eq!(
            tokenize(r"a\*\?\\\}\/").unwrap(),
            vec![
                Tok::Lit('a'),
                Tok::Lit('*'),
                Tok::Lit('?'),
                Tok::Lit('\\'),
                Tok::Lit('}'),
                Tok::Lit('/')
            ]
        );
        assert!(tokenize(r"\a").is_err());
        assert!(tokenize("\\").is_err());
    }

    #[test]
    fn reverse_keeps_escapes() {
        assert_eq!(reverse_pattern(r"a\*c"), r"c\*a");
        assert_eq!(reverse_pattern("abc"), "cba");
        assert_eq!(reverse_pattern(""), "");
    }

    #[test]
    fn trims() {
        assert_eq!(trim_prefix("*/", "a/b/c", false).unwrap(), "b/c");
        assert_eq!(trim_prefix("*/", "a/b/c", true).unwrap(), "c");
        assert_eq!(trim_suffix("/*", "a/b/c", false).unwrap(), "a/b");
        assert_eq!(trim_suffix("/*", "a/b/c", true).unwrap(), "a");
    }

    #[test]
    fn replace_all_handles_empty_matches_and_templates() {
        let re = Pattern::compile("", true, false).unwrap();
        assert_eq!(replace_all(&re, "abc", "-"), "-a-b-c-");
        let re = Pattern::compile("b*", true, false).unwrap();
        assert_eq!(replace_all(&re, "abc", "<$0>"), "a<bc>");
        let re = Pattern::compile("b", true, false).unwrap();
        assert_eq!(replace_all(&re, "abcb", "$$${0}$1$x"), "a$bc$b");
        assert_eq!(replace_first(&re, "abcb", "$0"), "a$0cb");
    }
}
