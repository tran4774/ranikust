// Derived from tonistiigi/fsutil, https://github.com/tonistiigi/fsutil,
// filter.go at commit 83cac42c1c52 (the version BuildKit v0.34.0 uses).
// Copyright 2017 Tõnis Tiigi. Licensed under the MIT license.
// Modified for ranikust: the exclude-side directory pruning test only.

//! When a walk may skip an excluded directory, as BuildKit's context walk
//! (`fsutil`'s `filterFS.Walk`) decides it.

use super::{Matcher, Pattern};

/// Characters that make an exception more than a plain path prefix
/// (`patternChars` on Unix).
const PATTERN_CHARS: &[u8] = b"*[]?^\\";

/// `patternWithoutTrailingGlob`: the pattern without a trailing `/**`, then
/// without a trailing `/*`.
fn without_trailing_glob(pattern: &[u8]) -> &[u8] {
    let p = pattern.strip_suffix(b"/**").unwrap_or(pattern);
    p.strip_suffix(b"/*").unwrap_or(p)
}

/// fsutil's `onlyPrefixExcludeExceptions`: every exception is a plain path,
/// possibly followed by `/*` or `/**`.
pub(super) fn only_prefix_exceptions(patterns: &[Pattern]) -> bool {
    !patterns.iter().any(|p| {
        p.exclusion
            && without_trailing_glob(&p.cleaned)
                .iter()
                .any(|c| PATTERN_CHARS.contains(c))
    })
}

impl Matcher {
    /// Whether something below the excluded directory `dir` may be
    /// re-included by an exception, so that a walk must descend into it.
    /// `false` means the walk may skip the whole directory.
    ///
    /// This is BuildKit's rule, from `fsutil`: without exceptions an
    /// excluded directory is skipped; when every exception is a plain path
    /// (optionally ending in `/*` or `/**`), it is skipped unless an
    /// exception lies inside it; otherwise it is always walked. Paths found
    /// inside are then judged one by one with
    /// [`matches_using_parent`](Self::matches_using_parent).
    #[must_use]
    pub fn may_reinclude_below(&self, dir: impl AsRef<[u8]>) -> bool {
        if !self.only_prefix_exceptions {
            return true;
        }
        if !self.exclusions {
            return false;
        }
        let dir = dir.as_ref();
        self.patterns.iter().filter(|p| p.exclusion).any(|p| {
            // `patStr + "/"` starts with `dir + "/"`.
            let pat = without_trailing_glob(&p.cleaned);
            pat.starts_with(dir) && pat.get(dir.len()).is_none_or(|&b| b == b'/')
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Matcher;

    #[test]
    fn prefix_exceptions_decide() {
        let m = Matcher::new(&["docs", "!docs/keep.md", "!other/**"]).unwrap();
        assert!(m.may_reinclude_below("docs"));
        assert!(!m.may_reinclude_below("doc"));
        assert!(!m.may_reinclude_below("docs/sub"));
        assert!(m.may_reinclude_below("other"));
        assert!(!m.may_reinclude_below("node_modules"));
    }

    #[test]
    fn no_exceptions_skip() {
        let m = Matcher::new(&["docs"]).unwrap();
        assert!(!m.may_reinclude_below("docs"));
    }

    #[test]
    fn wildcard_exceptions_always_descend() {
        for exception in [
            "!*/keep.md",
            "!docs/**/keep.md",
            "!d?cs",
            "!\\docs",
            "![d]ocs",
            "!^x",
        ] {
            let m = Matcher::new(&["*", exception]).unwrap();
            assert!(m.may_reinclude_below("anything"), "{exception}");
        }
    }
}
