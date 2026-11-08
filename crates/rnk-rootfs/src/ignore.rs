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
use std::path::{Component, Path, PathBuf};

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
    /// Components of the longest glob entry, which matches no more of a
    /// path's leading names.
    glob_parts: usize,
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
        self.glob_parts = self.glob_parts.max(parts.len());
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
        if self.is_empty() {
            return false;
        }
        path::clean(p.as_ref())
            .and_then(|c| self.lead(&c).map(|lead| self.covers_lead(&lead)))
            .unwrap_or(false)
    }

    /// [`IgnoreList::covers`] for a path a walk built from the names it
    /// read: no `..` in it (`.`, a leading `/` and doubled or trailing
    /// slashes are read as `covers` reads them).
    ///
    /// It reads only as many leading names as the longest entry has, and
    /// one more, so a walk asking about each entry takes time linear in its
    /// entries however deep they lie, where cleaning each whole path would
    /// take the square of the depth. A `..` among those names is cleaned as
    /// `covers` cleans it; one past them is not seen: give any path that
    /// may hold one to `covers`.
    #[must_use]
    pub fn covers_walked(&self, p: impl AsRef<Path>) -> bool {
        if self.is_empty() {
            return false;
        }
        let p = p.as_ref();
        match self.lead(p) {
            Some(lead) => self.covers_lead(&lead),
            None => self.covers(p),
        }
    }

    /// Whether an entry lies strictly below `p`: removing `p` whole would
    /// remove something ignored (kaniko's `childDirInIgnoreList`).
    #[must_use]
    pub fn has_below(&self, p: impl AsRef<Path>) -> bool {
        if self.is_empty() {
            return false;
        }
        path::clean(p.as_ref())
            .and_then(|c| self.lead(&c).map(|lead| self.has_below_lead(&lead)))
            .unwrap_or(false)
    }

    /// [`IgnoreList::has_below`] for a path a walk built, read as
    /// [`IgnoreList::covers_walked`] reads it.
    pub(crate) fn has_below_walked(&self, p: impl AsRef<Path>) -> bool {
        if self.is_empty() {
            return false;
        }
        let p = p.as_ref();
        match self.lead(p) {
            Some(lead) => self.has_below_lead(&lead),
            None => self.has_below(p),
        }
    }

    /// The leading names of `p` as [`path::clean`] reads them, as many as
    /// the longest entry has; `None` when a `..` comes before that many,
    /// which only cleaning the whole path resolves.
    fn lead<'p>(&self, p: &'p Path) -> Option<Lead<'p>> {
        let reach = self.max_parts.max(self.glob_parts);
        let mut names = Vec::new();
        for c in p.components() {
            let name = match c {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => return None,
                Component::Normal(n) => n,
                Component::Prefix(n) => n.as_os_str(),
            };
            if names.len() == reach {
                return Some(Lead { names, more: true });
            }
            names.push(name);
        }
        Some(Lead { names, more: false })
    }

    fn covers_lead(&self, lead: &Lead<'_>) -> bool {
        if !self.paths.is_empty() || !self.below.is_empty() {
            let mut prefix = PathBuf::new();
            for (i, name) in lead.names.iter().take(self.max_parts).enumerate() {
                prefix.push(name);
                let whole = i + 1 == lead.names.len() && !lead.more;
                if self.paths.contains(&prefix) || (!whole && self.below.contains(&prefix)) {
                    return true;
                }
            }
        }
        // `lead` holds as many names as the longest glob has, so it holds
        // fewer than a glob's only when the path does.
        self.globs.iter().any(|g| g.covers(&lead.names))
    }

    fn has_below_lead(&self, lead: &Lead<'_>) -> bool {
        // An entry strictly below a path has more names than the path, and
        // none has more than `lead` can hold.
        if lead.more {
            return false;
        }
        let c: PathBuf = lead.names.iter().collect();
        let strictly_below = |e: &PathBuf| e != &c && e.starts_with(&c);
        if self.paths.iter().chain(&self.below).any(strictly_below) {
            return true;
        }
        self.globs.iter().any(|g| g.has_below(&lead.names))
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

/// A path's leading names, as many as the list's longest entry has.
struct Lead<'p> {
    names: Vec<&'p OsStr>,
    /// The path has more names than `names` holds.
    more: bool,
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

/// Matches a whole component in O(tokens × characters) time: the iterative
/// wildcard algorithm. Every token but `*` takes exactly one character; on
/// a mismatch only the last `*` is widened by one character, never an
/// earlier one, which gives the same answer as trying every split (a later
/// star can absorb whatever an earlier one would have) without the
/// exponential cost. A component never holds `/`, so nothing is crossed.
/// Names come from images and `RUN` steps: the time must not depend on how
/// many stars meet a long name.
fn match_from(toks: &[Tok], name: &[char]) -> bool {
    let (mut t, mut n) = (0, 0);
    // After the last `*` seen: the token that follows it, and the first
    // character it has not absorbed yet.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match toks.get(t) {
            Some(Tok::Star) => {
                star = Some((t + 1, n));
                t += 1;
            }
            Some(tok) if takes(tok, name[n]) => {
                t += 1;
                n += 1;
            }
            _ => match star {
                Some((after, from)) => {
                    star = Some((after, from + 1));
                    t = after;
                    n = from + 1;
                }
                None => return false,
            },
        }
    }
    toks.get(t..)
        .is_some_and(|rest| rest.iter().all(|tok| *tok == Tok::Star))
}

/// Whether a one-character token takes `c`; `*` takes nothing here.
fn takes(tok: &Tok, c: char) -> bool {
    match tok {
        Tok::Star => false,
        Tok::Any => true,
        Tok::Lit(l) => *l == c,
        Tok::Class { negated, ranges } => {
            ranges.iter().any(|(lo, hi)| (lo..=hi).contains(&&c)) != *negated
        }
    }
}

#[cfg(test)]
mod tests;
