//! Paths inside a root that deletion, extraction and the kaniko-mode walk
//! leave alone.
//!
//! Matching follows kaniko's `hasCleanedFilepathPrefix`
//! (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1261-1281`): a path is ignored
//! when its leading components match an entry's components, each by a
//! `filepath.Match` glob; an entry marked "below only" (kaniko's
//! `PrefixMatchOnly`) ignores what is strictly below it, never the entry
//! itself.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::path;

/// Paths inside a root to leave alone, with everything below them.
/// Matching is on lexically cleaned paths: `/a/./b/` covers `a/b` and
/// `a/b/c`, not `a/bc`.
///
/// Paths are physical: they are compared with the path a walk reaches, and
/// a walk never follows a symlink. With `/var/run -> /run`, ignoring
/// `/var/run` ignores the symlink, not `/run`; list `/run` (mountinfo gives
/// physical paths).
#[derive(Clone, Debug, Default)]
pub struct IgnoreList {
    /// Literal entries: the path and everything below it.
    paths: HashSet<PathBuf>,
    /// Literal entries that ignore only what is strictly below them.
    below: HashSet<PathBuf>,
    /// Entries with glob characters, per component.
    globs: Vec<Glob>,
    /// Components of the longest literal entry: a path's prefix with more
    /// components can equal no literal entry, so [`IgnoreList::covers`]
    /// looks at no longer prefix.
    max_parts: usize,
}

impl IgnoreList {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `p`, a path inside the root, absolute or relative, taken
    /// literally: it and everything below it are ignored.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPath`] when `p` names the root itself or climbs above
    /// it with `..`.
    pub fn insert(&mut self, p: impl AsRef<Path>) -> Result<()> {
        let c = cleaned(p.as_ref())?;
        self.note_parts(&c);
        self.paths.insert(c);
        Ok(())
    }

    fn note_parts(&mut self, c: &Path) {
        self.max_parts = self.max_parts.max(c.iter().count());
    }

    /// Adds `p`, taken literally, ignoring only what is strictly below it:
    /// kaniko's `PrefixMatchOnly` (`/tmp/apt-key-gpghome`).
    ///
    /// # Errors
    ///
    /// As [`IgnoreList::insert`].
    pub fn insert_below(&mut self, p: impl AsRef<Path>) -> Result<()> {
        let c = cleaned(p.as_ref())?;
        self.note_parts(&c);
        self.below.insert(c);
        Ok(())
    }

    /// Adds `p` as kaniko adds an `--ignore-path`: each component is a
    /// pattern of Go's `filepath.Match` (`*`, `?`, `[a-z]`, `[^a]`, `\`
    /// escapes), so `/var/cache/*` ignores what is in `/var/cache` but not
    /// the directory itself. A path without pattern characters is literal,
    /// as [`IgnoreList::insert`].
    ///
    /// # Errors
    ///
    /// As [`IgnoreList::insert`], and [`Error::InvalidPath`] for a
    /// malformed pattern (kaniko's `ErrBadPattern`, which makes kaniko
    /// ignore nothing for it).
    pub fn insert_glob(&mut self, p: impl AsRef<Path>) -> Result<()> {
        let p = p.as_ref();
        let c = cleaned(p)?;
        if !c
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|b| b"*?[\\".contains(b))
        {
            self.note_parts(&c);
            self.paths.insert(c);
            return Ok(());
        }
        let parts = c
            .iter()
            .map(|name| {
                Pattern::compile(name).ok_or_else(|| Error::InvalidPath {
                    path: p.to_owned(),
                    reason: "a malformed glob pattern",
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.globs.push(Glob { parts });
        Ok(())
    }

    /// Whether `p` (cleaned the same way) is exactly a literal entry added
    /// with [`IgnoreList::insert`].
    #[must_use]
    pub fn contains(&self, p: impl AsRef<Path>) -> bool {
        path::clean(p.as_ref()).is_some_and(|c| self.paths.contains(&c))
    }

    /// Whether `p` is ignored: it is, or lies below, an entry (strictly
    /// below, for [`IgnoreList::insert_below`]). A path climbing above the
    /// root is not.
    #[must_use]
    pub fn covers(&self, p: impl AsRef<Path>) -> bool {
        let Some(c) = path::clean(p.as_ref()) else {
            return false;
        };
        if c.as_os_str().is_empty() {
            return false;
        }
        // The cleaned path is `a/b/c`: its leading components are the bytes
        // before each `/`, and the whole path. Only prefixes of at most
        // `max_parts` components can equal a literal entry, so a deep path
        // costs its length once, not its depth times its length.
        let bytes = c.as_os_str().as_bytes();
        let ends = bytes
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| (b == b'/').then_some(i))
            .chain(std::iter::once(bytes.len()))
            .take(self.max_parts);
        for end in ends {
            let a = Path::new(OsStr::from_bytes(bytes.get(..end).unwrap_or_default()));
            let whole = end == bytes.len();
            if self.paths.contains(a) || (!whole && self.below.contains(a)) {
                return true;
            }
        }
        if self.globs.is_empty() {
            return false;
        }
        let names: Vec<&OsStr> = c.iter().collect();
        self.globs.iter().any(|g| g.covers(&names))
    }

    /// Whether an entry lies strictly below `p`: removing `p` whole would
    /// remove something ignored (kaniko's `childDirInIgnoreList`).
    #[must_use]
    pub fn has_below(&self, p: impl AsRef<Path>) -> bool {
        let Some(c) = path::clean(p.as_ref()) else {
            return false;
        };
        let strictly_below = |e: &PathBuf| e != &c && e.starts_with(&c);
        if self.paths.iter().chain(&self.below).any(strictly_below) {
            return true;
        }
        let names: Vec<&OsStr> = c.iter().collect();
        self.globs.iter().any(|g| g.has_below(&names))
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len() + self.below.len() + self.globs.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `p` cleaned, refused when it is the root or leaves it.
fn cleaned(p: &Path) -> Result<PathBuf> {
    match path::clean(p) {
        Some(c) if !c.as_os_str().is_empty() => Ok(c),
        Some(_) => Err(Error::InvalidPath {
            path: p.to_owned(),
            reason: "an ignore-list entry cannot be the root itself",
        }),
        None => Err(Error::InvalidPath {
            path: p.to_owned(),
            reason: "an ignore-list entry cannot leave the root",
        }),
    }
}

/// An entry with glob characters: one pattern per component.
#[derive(Clone, Debug)]
struct Glob {
    parts: Vec<Pattern>,
}

impl Glob {
    /// The path `names` has at least as many components and its leading
    /// ones match.
    fn covers(&self, names: &[&OsStr]) -> bool {
        names.len() >= self.parts.len() && self.parts.iter().zip(names).all(|(p, n)| p.matches(n))
    }

    /// The entry is longer than `names`, which match its leading patterns.
    fn has_below(&self, names: &[&OsStr]) -> bool {
        self.parts.len() > names.len() && self.parts.iter().zip(names).all(|(p, n)| p.matches(n))
    }
}

/// One component's pattern, as Go's `filepath.Match` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern(Vec<Tok>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Lit(char),
    /// `?`: one character.
    Any,
    /// `*`: any run of characters.
    Star,
    /// `[...]`, negated with `^`.
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Pattern {
    /// Compiles one component; `None` for a malformed pattern.
    fn compile(name: &OsStr) -> Option<Self> {
        let chars: Vec<char> = name.to_string_lossy().chars().collect();
        let mut toks = Vec::new();
        let mut i = 0;
        while let Some(&c) = chars.get(i) {
            i += 1;
            match c {
                '*' => toks.push(Tok::Star),
                '?' => toks.push(Tok::Any),
                '\\' => {
                    toks.push(Tok::Lit(*chars.get(i)?));
                    i += 1;
                }
                '[' => {
                    let negated = chars.get(i) == Some(&'^');
                    if negated {
                        i += 1;
                    }
                    let mut ranges = Vec::new();
                    loop {
                        if chars.get(i) == Some(&']') && !ranges.is_empty() {
                            i += 1;
                            break;
                        }
                        let lo = class_char(&chars, &mut i)?;
                        let hi = if chars.get(i) == Some(&'-') {
                            i += 1;
                            class_char(&chars, &mut i)?
                        } else {
                            lo
                        };
                        if lo > hi {
                            return None;
                        }
                        ranges.push((lo, hi));
                    }
                    toks.push(Tok::Class { negated, ranges });
                }
                c => toks.push(Tok::Lit(c)),
            }
        }
        Some(Self(toks))
    }

    fn matches(&self, name: &OsStr) -> bool {
        let chars: Vec<char> = name.to_string_lossy().chars().collect();
        match_from(&self.0, &chars)
    }
}

/// One character of a class, Go's `getEsc`: `-` and `]` must be escaped.
fn class_char(chars: &[char], i: &mut usize) -> Option<char> {
    let c = *chars.get(*i)?;
    *i += 1;
    match c {
        '-' | ']' => None,
        '\\' => {
            let e = *chars.get(*i)?;
            *i += 1;
            Some(e)
        }
        c => Some(c),
    }
}

/// Backtracking match of a whole component. Patterns come from the
/// operator and components are at most 255 bytes.
fn match_from(toks: &[Tok], name: &[char]) -> bool {
    let Some((tok, rest)) = toks.split_first() else {
        return name.is_empty();
    };
    match tok {
        Tok::Star => (0..=name.len()).any(|k| name.get(k..).is_some_and(|n| match_from(rest, n))),
        Tok::Any => name.split_first().is_some_and(|(_, n)| match_from(rest, n)),
        Tok::Lit(c) => name
            .split_first()
            .is_some_and(|(h, n)| h == c && match_from(rest, n)),
        Tok::Class { negated, ranges } => name.split_first().is_some_and(|(h, n)| {
            let inside = ranges.iter().any(|(lo, hi)| (lo..=hi).contains(&h));
            inside != *negated && match_from(rest, n)
        }),
    }
}

#[cfg(test)]
mod tests;
