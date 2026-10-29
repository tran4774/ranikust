// Derived from moby/patternmatcher, https://github.com/moby/patternmatcher,
// patternmatcher.go at tag v0.6.1 (commit 5a6d8429a19b).
// Copyright 2012-2017 Docker, Inc. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: byte-string paths, patterns compiled when the
// matcher is built, Go regexps translated for the `regex` crate.

//! [`Matcher`], a port of `patternmatcher.PatternMatcher`.

mod walk;

#[cfg(test)]
mod tests;

use regex::{Regex, RegexBuilder};

use crate::Error;
use crate::golang::{self, BadPattern};
use crate::goregexp;

/// Compile-size limit for one pattern's regex, as Go's parser allows
/// 128 MB of instructions. The `regex` default (10 MiB) would reject a few
/// `?{1000}` in one line, which Go accepts.
const REGEX_SIZE_LIMIT: usize = 128 << 20;

/// Compiled `.dockerignore` patterns: a port of moby `patternmatcher`
/// v0.6.1's `PatternMatcher`.
///
/// Paths and patterns are byte strings with `/` as the separator, like Go
/// strings; `&str`, `String` and `&[u8]` are all accepted. Paths are
/// relative to the context root and normally clean (`filepath.Clean`);
/// matching does not clean them, as `PatternMatcher`'s methods do not.
///
/// Building the matcher compiles every pattern, so matching does no
/// parsing; it still reports [`Error::BadPattern`] when it reaches a
/// pattern that Go would accept in `New` but fail to compile later, which
/// `patternmatcher` validates only partially. A matcher holds no state
/// between calls and can be shared between threads.
// [open: Q-010] paths and patterns are bytes (`AsRef<[u8]>`), not `&str`.
#[derive(Debug, Clone)]
pub struct Matcher {
    patterns: Vec<Pattern>,
    exclusions: bool,
    /// fsutil's `onlyPrefixExcludeExceptions`; see [`Matcher::may_reinclude_below`].
    only_prefix_exceptions: bool,
}

/// One pattern: `patternmatcher.Pattern`.
#[derive(Debug, Clone)]
struct Pattern {
    /// The cleaned pattern without its leading `!`.
    cleaned: Vec<u8>,
    exclusion: bool,
    /// The compiled form, or the error every match with it reports.
    kind: Result<MatchType, BadPattern>,
}

#[derive(Debug, Clone)]
enum MatchType {
    /// The path equals the pattern.
    Exact,
    /// The pattern ends with `**` and has no other special character: the
    /// path starts with the rest of the pattern.
    Prefix,
    /// The pattern starts with `**`: the path ends with the rest.
    Suffix,
    Regexp(Regex),
}

/// What a directory matched, for its children: `patternmatcher.MatchInfo`.
///
/// The default value carries no information; [`Matcher::matches_using_parent`]
/// then also checks the path's parent directories itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchInfo {
    parent_matched: Vec<bool>,
}

impl Matcher {
    /// Compiles `patterns`, as `patternmatcher.New` does: each is trimmed
    /// and cleaned, empty ones are skipped, and a leading `!` makes an
    /// exception. Pass the output of [`read_patterns`](crate::read_patterns)
    /// for a `.dockerignore` file.
    ///
    /// # Errors
    ///
    /// [`Error::IllegalExclusion`] for a lone `!`, and
    /// [`Error::BadPattern`] where `filepath.Match` reports a syntax error.
    pub fn new<P: AsRef<[u8]>>(patterns: &[P]) -> Result<Self, Error> {
        let mut out = Vec::with_capacity(patterns.len());
        let mut exclusions = false;
        for p in patterns {
            let p = golang::trim_space(p.as_ref());
            if p.is_empty() {
                continue;
            }
            let mut cleaned = golang::clean(p);
            let mut exclusion = false;
            if cleaned.first() == Some(&b'!') {
                if cleaned.len() == 1 {
                    return Err(Error::IllegalExclusion);
                }
                exclusion = true;
                cleaned.remove(0);
                exclusions = true;
            }
            // Go validates the syntax by matching against ".", which only
            // checks the pattern as far as that match gets.
            if golang::match_path(&cleaned, b".").is_err() {
                return Err(Error::bad_pattern(&cleaned));
            }
            let kind = compile(&cleaned).map(|(kind, _)| kind);
            out.push(Pattern {
                cleaned,
                exclusion,
                kind,
            });
        }
        let only_prefix_exceptions = walk::only_prefix_exceptions(&out);
        Ok(Self {
            patterns: out,
            exclusions,
            only_prefix_exceptions,
        })
    }

    /// Whether `path` is excluded: it, or one of its parent directories,
    /// matches a pattern, and no later exception matches it or one of its
    /// parents (`PatternMatcher.MatchesOrParentMatches`).
    ///
    /// # Errors
    ///
    /// [`Error::BadPattern`] when a pattern that must be evaluated does not
    /// compile.
    pub fn matches_or_parent_matches(&self, path: impl AsRef<[u8]>) -> Result<bool, Error> {
        let file = path.as_ref();
        let parent = golang::dir(file);
        let mut matched = false;
        for pattern in &self.patterns {
            // An inclusion cannot change a match, nor an exception a
            // non-match.
            if pattern.exclusion != matched {
                continue;
            }
            let mut m = pattern.matches(file)?;
            if !m && parent != b"." {
                m = pattern.matches_a_parent(&parent);
            }
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok(matched)
    }

    /// [`matches_or_parent_matches`](Self::matches_or_parent_matches) for
    /// a walk: `parent` is the [`MatchInfo`] returned for `path`'s parent
    /// directory, or the default for a path at the top of the context. The
    /// returned `MatchInfo` is for `path`'s children
    /// (`PatternMatcher.MatchesUsingParentResults`).
    ///
    /// This is what BuildKit's context walk calls. It can differ from
    /// `matches_or_parent_matches` when exceptions are involved: a pattern
    /// skipped for the parent is not reconsidered through the parent for
    /// the child [open: Q-050].
    ///
    /// # Errors
    ///
    /// [`Error::BadPattern`] as for `matches_or_parent_matches`, and
    /// [`Error::MatchInfoMismatch`] when `parent` comes from another matcher.
    pub fn matches_using_parent(
        &self,
        path: impl AsRef<[u8]>,
        parent: &MatchInfo,
    ) -> Result<(bool, MatchInfo), Error> {
        let parent_matched = &parent.parent_matched;
        if !parent_matched.is_empty() && parent_matched.len() != self.patterns.len() {
            return Err(Error::MatchInfoMismatch);
        }
        let file = path.as_ref();
        let mut matched = false;
        let mut info = vec![false; self.patterns.len()];
        for ((i, pattern), slot) in self.patterns.iter().enumerate().zip(info.iter_mut()) {
            // If the parent matched this pattern, the child does too.
            let mut m = parent_matched.get(i).copied().unwrap_or(false);
            if !m {
                if pattern.exclusion != matched {
                    continue;
                }
                m = pattern.matches(file)?;
                // Without information about the parent, check the parent
                // directories here, as `matches_or_parent_matches` does.
                if !m && parent_matched.is_empty() {
                    let parent = golang::dir(file);
                    if parent != b"." {
                        m = pattern.matches_a_parent(&parent);
                    }
                }
            }
            *slot = m;
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok((
            matched,
            MatchInfo {
                parent_matched: info,
            },
        ))
    }

    /// Whether any pattern is an exception (`!`): `PatternMatcher.Exclusions`.
    #[must_use]
    pub fn has_exclusions(&self) -> bool {
        self.exclusions
    }
}

impl Pattern {
    /// `Pattern.match`.
    fn matches(&self, path: &[u8]) -> Result<bool, Error> {
        let kind = self
            .kind
            .as_ref()
            .map_err(|_| Error::bad_pattern(&self.cleaned))?;
        Ok(match kind {
            MatchType::Exact => path == self.cleaned.as_slice(),
            MatchType::Prefix => {
                // Strip the trailing `**`.
                let prefix = self
                    .cleaned
                    .get(..self.cleaned.len().saturating_sub(2))
                    .unwrap_or_default();
                path.starts_with(prefix)
            }
            MatchType::Suffix => {
                // Strip the leading `**` (two bytes, as Go slices it).
                let suffix = self.cleaned.get(2..).unwrap_or_default();
                // `**/foo` also matches `foo`.
                path.ends_with(suffix) || (suffix.first() == Some(&b'/') && path == &suffix[1..])
            }
            // Go's regexp reads an invalid byte as U+FFFD.
            MatchType::Regexp(re) => re.is_match(&golang::go_runes(path)),
        })
    }

    /// Whether the pattern matches one of the directories of `parent`
    /// (`a`, `a/b`, `a/b/c` for `a/b/c`), shortest first.
    fn matches_a_parent(&self, parent: &[u8]) -> bool {
        let ends = parent
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'/')
            .map(|(i, _)| i)
            .chain([parent.len()]);
        for end in ends {
            // Go ignores errors here: the same pattern already matched (or
            // failed on) the path itself.
            if self.matches(&parent[..end]).unwrap_or(false) {
                return true;
            }
        }
        false
    }
}

impl Error {
    fn bad_pattern(pattern: &[u8]) -> Self {
        Self::BadPattern {
            pattern: String::from_utf8_lossy(pattern).into_owned(),
        }
    }
}

/// `shouldEscape`: the characters `compile` escapes. Go's bitmap was meant
/// to cover `.+()|{}$`, but `|`, `{` and `}` fall outside it (their bit
/// index exceeds 7), so they reach the regexp unescaped.
fn should_escape(c: char) -> bool {
    matches!(c, '.' | '+' | '(' | ')' | '$')
}

/// `Pattern.compile`: the match type of a cleaned pattern and, for
/// [`MatchType::Regexp`], the Go regexp it stands for.
fn compile(pattern: &[u8]) -> Result<(MatchType, String), BadPattern> {
    #[derive(PartialEq, Eq)]
    enum Detected {
        Exact,
        Prefix,
        Suffix,
        Regexp,
    }
    // Go reads the pattern with text/scanner: invalid bytes become U+FFFD
    // and a byte order mark at the very start is skipped.
    let decoded = golang::go_runes(pattern);
    let runes: Vec<char> = decoded
        .strip_prefix('\u{FEFF}')
        .unwrap_or(&decoded)
        .chars()
        .collect();
    let mut reg = String::from("^");
    let mut detected = Detected::Exact;
    let mut idx = 0;
    let mut i = 0;
    while let Some(&ch) = runes.get(idx) {
        idx += 1;
        if ch == '*' {
            if runes.get(idx) == Some(&'*') {
                // Some flavour of `**`; `**/` counts as `**`.
                idx += 1;
                if runes.get(idx) == Some(&'/') {
                    idx += 1;
                }
                if idx == runes.len() {
                    // `**` at the end accepts everything, like .gitignore.
                    if detected == Detected::Exact {
                        detected = Detected::Prefix;
                    } else {
                        reg.push_str(".*");
                        detected = Detected::Regexp;
                    }
                } else {
                    // Any number of directories, even none.
                    reg.push_str("(.*/)?");
                    detected = Detected::Regexp;
                }
                if i == 0 {
                    detected = Detected::Suffix;
                }
            } else {
                // `*`: anything but `/`.
                reg.push_str("[^/]*");
                detected = Detected::Regexp;
            }
        } else if ch == '?' {
            // `?`: any character but `/`.
            reg.push_str("[^/]");
            detected = Detected::Regexp;
        } else if should_escape(ch) {
            reg.push('\\');
            reg.push(ch);
        } else if ch == '\\' {
            // Escapes the next character; a trailing `\` stays as is.
            if let Some(&next) = runes.get(idx) {
                idx += 1;
                reg.push('\\');
                reg.push(next);
                detected = Detected::Regexp;
            } else {
                reg.push('\\');
            }
        } else if ch == '[' || ch == ']' {
            reg.push(ch);
            detected = Detected::Regexp;
        } else {
            reg.push(ch);
        }
        i += 1;
    }
    let kind = match detected {
        Detected::Exact => MatchType::Exact,
        Detected::Prefix => MatchType::Prefix,
        Detected::Suffix => MatchType::Suffix,
        Detected::Regexp => {
            reg.push('$');
            let translated = goregexp::translate(&reg)?;
            let re = RegexBuilder::new(&translated)
                .size_limit(REGEX_SIZE_LIMIT)
                .build()
                .map_err(|_| BadPattern)?;
            MatchType::Regexp(re)
        }
    };
    Ok((kind, reg))
}
