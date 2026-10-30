//! Which files a `COPY` or `ADD` source names: in the build context,
//! filtered by `.dockerignore`, or in another stage's root.
//!
//! This crate resolves sources (Q-080, `ADR-0032`), against the context
//! walk's entries (so an ignored file is never named or read, not even
//! through a symlink), with BuildKit's wildcard and error rules
//! ([`crate::glob`]): a plain source that is missing is an error; a
//! wildcard that matches nothing copies nothing, as BuildKit's
//! `AllowEmptyWildcard` (`buildkit@cd8ee244
//! frontend/dockerfile/dockerfile2llb/convert_copy.go:291-292`,
//! `solver/llbsolver/file/backend.go:309-312`), unless the directory before
//! the first wildcard is missing, which fsutil's walk reports.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rnk_context::Context;
use rnk_ignore::Matcher;
use rnk_rootfs::{FileType, OpenHow, RootPath};

use crate::error::{Error, Result};
use crate::glob;

/// What a kept context entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    /// A symlink, with its target as read during the walk.
    Symlink(PathBuf),
    Other,
}

/// The most symlinks followed for one source, as Linux's `MAXSYMLINKS`.
const MAX_HOPS: usize = 40;

/// One resolved `COPY`/`ADD` source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The entry to copy, relative to its root, with no symlink in it (for
    /// the context): what is read.
    pub(crate) path: PathBuf,
    /// The source as named, after wildcards: its last name names the copy.
    pub(crate) named: PathBuf,
    /// Whether `path` is a directory.
    pub(crate) dir: bool,
}

/// The context's entries that `.dockerignore` keeps, with their kind;
/// walked once per build.
#[derive(Debug, Default)]
pub struct ContextFiles {
    entries: BTreeMap<PathBuf, Kind>,
}

impl ContextFiles {
    /// Walks `context` with `ignore`, reading each kept symlink's target.
    ///
    /// # Errors
    ///
    /// [`Error::Context`] when the walk fails, [`Error::Rootfs`] when a
    /// symlink cannot be read.
    pub fn load(context: &Context, ignore: Option<&Matcher>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for entry in context.walk(ignore) {
            let entry = entry?;
            // The directories leading to a kept entry are kept too, as
            // BuildKit's filtered walk reports them.
            for a in entry.path.ancestors().skip(1) {
                if !a.as_os_str().is_empty() {
                    entries.insert(a.to_owned(), Kind::Dir);
                }
            }
            let kind = match entry.meta.file_type {
                FileType::Directory => Kind::Dir,
                FileType::Symlink => Kind::Symlink(context.root().read_link(&entry.path)?),
                FileType::Regular
                | FileType::Fifo
                | FileType::CharDevice
                | FileType::BlockDevice
                | FileType::Socket
                | FileType::Unknown => Kind::Other,
            };
            entries.insert(entry.path, kind);
        }
        Ok(Self { entries })
    }

    /// Whether `path`, relative to the context root, is kept; the root
    /// always is.
    pub(crate) fn keeps(&self, path: &Path) -> bool {
        path.as_os_str().is_empty() || self.entries.contains_key(path)
    }

    /// The kept entries `source` names, sorted: one for a plain path, those
    /// at its depth for a wildcard pattern. Paths are relative to the
    /// context root; `""` is the root.
    ///
    /// A symlink is followed inside the context, as BuildKit follows a
    /// source symlink (Q-120), but only through kept entries: every
    /// link on the way and the target must be kept, so `.dockerignore`
    /// cannot be bypassed by a link to an ignored file or directory.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a plain source, or the directory before a
    /// wildcard, is not kept; a symlink leads to an ignored or missing entry
    /// or loops; or the pattern is malformed. A wildcard matching nothing is
    /// no error: the result is empty.
    pub(crate) fn resolve(&self, source: &str) -> Result<Vec<Resolved>> {
        let clean = clean_source(source);
        if clean.as_os_str().is_empty() {
            return Ok(vec![Resolved {
                path: PathBuf::new(),
                named: PathBuf::new(),
                dir: true,
            }]);
        }
        let text = clean.to_string_lossy();
        let named: Vec<PathBuf> = if glob::has_wildcard(&text) {
            let parts: Vec<&str> = text.split('/').collect();
            let mut out = Vec::new();
            for path in self.entries.keys() {
                let names: Vec<_> = path.components().collect();
                if names.len() != parts.len() {
                    continue;
                }
                let mut all = true;
                for (pattern, name) in parts.iter().zip(&names) {
                    let name = name.as_os_str().to_string_lossy();
                    if !glob::matches(pattern, &name).map_err(|_| bad_pattern(source))? {
                        all = false;
                        break;
                    }
                }
                if all {
                    out.push(path.clone());
                }
            }
            out
        } else {
            vec![clean.clone()]
        };
        if named.is_empty() {
            let base = wildcard_base(&text);
            if !base.as_os_str().is_empty() {
                // The directory the walk would start in must be there.
                let (_, dir) = self.follow(&base, source)?;
                if !dir {
                    return Err(not_found(source));
                }
            }
            return Ok(Vec::new());
        }
        named
            .into_iter()
            .map(|n| {
                let (path, dir) = self.follow(&n, source)?;
                Ok(Resolved {
                    path,
                    named: n,
                    dir,
                })
            })
            .collect()
    }

    /// `path` with every symlink in it resolved inside the context, through
    /// kept entries only; and whether it is a directory.
    fn follow(&self, path: &Path, source: &str) -> Result<(PathBuf, bool)> {
        let mut pending: Vec<std::ffi::OsString> = path
            .components()
            .rev()
            .map(|c| c.as_os_str().to_owned())
            .collect();
        let mut at = PathBuf::new();
        let mut hops = 0;
        while let Some(name) = pending.pop() {
            let next = at.join(&name);
            match self.entries.get(&next) {
                None => return Err(not_found(source)),
                Some(Kind::Symlink(target)) => {
                    hops += 1;
                    if hops > MAX_HOPS {
                        return Err(Error::Invalid(format!(
                            "source {source:?}: too many levels of symlinks"
                        )));
                    }
                    // Absolute targets start at the context root; `..`
                    // never climbs above it.
                    let joined = if target.is_absolute() {
                        target.clone()
                    } else {
                        at.join(target)
                    };
                    let mut rest: Vec<_> = clean_source(&joined.to_string_lossy())
                        .components()
                        .rev()
                        .map(|c| c.as_os_str().to_owned())
                        .collect();
                    pending.append(&mut rest);
                    at = PathBuf::new();
                }
                Some(Kind::Dir) => at = next,
                Some(Kind::Other) => {
                    if !pending.is_empty() {
                        return Err(not_found(source));
                    }
                    return Ok((next, false));
                }
            }
        }
        Ok((at, true))
    }
}

/// The entries of `root` (a stage's or an image's) that `source` names,
/// sorted, with whether each is a directory. A plain path is returned as
/// it is, resolved when copied; a wildcard pattern is matched one directory
/// level at a time, listing directories through `root`.
///
/// # Errors
///
/// [`Error::Invalid`] when a plain source or the directory before the
/// first wildcard is missing, or the pattern is malformed; a wildcard
/// matching nothing gives an empty result. [`Error::Rootfs`] when a
/// directory cannot be listed.
pub(crate) fn resolve_in_root(root: &RootPath, source: &str) -> Result<Vec<Resolved>> {
    let clean = clean_source(source);
    let text = clean.to_string_lossy().into_owned();
    if !glob::has_wildcard(&text) {
        let dir = match root.open_at(&clean, &OpenHow::path().directory()) {
            Ok(_) => true,
            Err(e) if e.raw_os_error() == Some(libc_enotdir()) => false,
            Err(e) if e.raw_os_error() == Some(libc_enoent()) => return Err(not_found(source)),
            Err(e) => return Err(e.into()),
        };
        return Ok(vec![Resolved {
            path: clean.clone(),
            named: clean,
            dir,
        }]);
    }
    let mut found = vec![PathBuf::new()];
    let parts: Vec<&str> = text.split('/').collect();
    for (i, pattern) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let mut next = Vec::new();
        for dir in &found {
            if !glob::has_wildcard(pattern) {
                next.push(dir.join(pattern));
                continue;
            }
            for name in list_dir(root, dir)? {
                let n = name.to_string_lossy();
                if glob::matches(pattern, &n).map_err(|_| bad_pattern(source))? {
                    next.push(dir.join(&name));
                }
            }
        }
        if !last {
            // Only directories lead further; a missing name or a file ends
            // that branch, any other error stops the copy.
            let mut dirs = Vec::with_capacity(next.len());
            for p in next {
                match root.open_at(&p, &OpenHow::path().directory()) {
                    Ok(_) => dirs.push(p),
                    Err(e) if matches!(e.raw_os_error(), Some(c) if c == libc_enoent() || c == libc_enotdir()) =>
                        {}
                    Err(e) => return Err(e.into()),
                }
            }
            next = dirs;
        }
        found = next;
    }
    let mut out = Vec::new();
    for p in found {
        match root.open_at(&p, &OpenHow::path().directory()) {
            Ok(_) => out.push(Resolved {
                path: p.clone(),
                named: p,
                dir: true,
            }),
            Err(e) if e.raw_os_error() == Some(libc_enotdir()) => out.push(Resolved {
                path: p.clone(),
                named: p,
                dir: false,
            }),
            // A literal last component that does not exist.
            Err(e) if e.raw_os_error() == Some(libc_enoent()) => {}
            Err(e) => return Err(e.into()),
        }
    }
    out.sort_by(|a, b| a.named.cmp(&b.named));
    if out.is_empty() {
        let base = wildcard_base(&text);
        match root.open_at(&base, &OpenHow::path().directory()) {
            Ok(_) => {}
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == libc_enoent() || c == libc_enotdir()) =>
            {
                return Err(not_found(source));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

/// The components of a cleaned pattern before its first wildcard, as
/// fsutil's `splitWildcards` (`fsutil copy/copy.go:687-705`).
fn wildcard_base(pattern: &str) -> PathBuf {
    pattern
        .split('/')
        .take_while(|c| !glob::has_wildcard(c))
        .collect()
}

/// The names in the directory `dir` of `root`, sorted.
fn list_dir(root: &RootPath, dir: &Path) -> Result<Vec<OsString>> {
    let fd = match root.open_at(dir, &OpenHow::read().directory()) {
        Ok(fd) => fd,
        Err(e) if matches!(e.raw_os_error(), Some(c) if c == libc_enoent() || c == libc_enotdir()) =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.into()),
    };
    let entries = rustix::fs::Dir::read_from(&fd).map_err(|e| Error::Io {
        op: "list",
        path: dir.to_owned(),
        source: e.into(),
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io {
            op: "list",
            path: dir.to_owned(),
            source: e.into(),
        })?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from(std::ffi::OsStr::from_bytes(name)));
        }
    }
    names.sort();
    Ok(names)
}

fn libc_enoent() -> i32 {
    rustix::io::Errno::NOENT.raw_os_error()
}

fn libc_enotdir() -> i32 {
    rustix::io::Errno::NOTDIR.raw_os_error()
}

/// A source path relative to its root, cleaned as BuildKit cleans
/// `"/" + source`: `.` and `..` resolved lexically, never above the root.
pub(crate) fn clean_source(source: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(source).components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

fn not_found(source: &str) -> Error {
    Error::Invalid(format!(
        "source {source:?}: not found in the build context or stage"
    ))
}

fn bad_pattern(source: &str) -> Error {
    Error::Invalid(format!("source {source:?}: malformed wildcard pattern"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[(&str, Kind)]) -> ContextFiles {
        ContextFiles {
            entries: paths
                .iter()
                .map(|(p, k)| (PathBuf::from(p), k.clone()))
                .collect(),
        }
    }

    fn link(t: &str) -> Kind {
        Kind::Symlink(PathBuf::from(t))
    }

    fn paths(r: Vec<Resolved>) -> Vec<(String, bool)> {
        r.into_iter()
            .map(|r| (r.path.to_string_lossy().into_owned(), r.dir))
            .collect()
    }

    #[test]
    fn sources_are_cleaned_inside_the_root() {
        assert_eq!(clean_source("/a/./b/../c"), PathBuf::from("a/c"));
        assert_eq!(clean_source("../../x"), PathBuf::from("x"));
        assert_eq!(clean_source("."), PathBuf::new());
    }

    #[test]
    fn plain_paths_and_wildcards_match_kept_entries() {
        let f = files(&[
            ("a.js", Kind::Other),
            ("b.js", Kind::Other),
            ("src", Kind::Dir),
            ("src/c.js", Kind::Other),
        ]);
        assert_eq!(
            paths(f.resolve("./a.js").unwrap()),
            [("a.js".into(), false)]
        );
        assert_eq!(
            paths(f.resolve("*.js").unwrap()),
            [("a.js".into(), false), ("b.js".into(), false)]
        );
        // A wildcard matches at its own depth only.
        assert_eq!(
            paths(f.resolve("*/c.js").unwrap()),
            [("src/c.js".into(), false)]
        );
        assert_eq!(paths(f.resolve(".").unwrap()), [(String::new(), true)]);
        assert!(matches!(f.resolve("missing"), Err(Error::Invalid(_))));
        // An empty wildcard copies nothing, as BuildKit; its base must be
        // there.
        assert_eq!(f.resolve("*.rs").unwrap(), []);
        assert_eq!(f.resolve("src/*.rs").unwrap(), []);
        assert!(matches!(f.resolve("lib/*.rs"), Err(Error::Invalid(_))));
        assert!(matches!(f.resolve("a.js/*"), Err(Error::Invalid(_))));
        assert!(matches!(f.resolve("[a"), Err(Error::Invalid(_))));
    }

    #[test]
    fn symlinks_resolve_through_kept_entries_only() {
        // `.env` and `secrets/` are ignored: not in the walk.
        let f = files(&[
            ("app", Kind::Dir),
            ("app/main.js", Kind::Other),
            ("to-env", link(".env")),
            ("to-secrets", link("secrets/")),
            ("to-app", link("./app")),
            ("abs", link("/app/main.js")),
            ("up", link("../../app")),
            ("via", link("to-app/main.js")),
            ("loop-a", link("loop-b")),
            ("loop-b", link("loop-a")),
        ]);
        assert!(
            matches!(f.resolve("to-env"), Err(Error::Invalid(_))),
            "file"
        );
        assert!(
            matches!(f.resolve("to-secrets"), Err(Error::Invalid(_))),
            "dir"
        );
        assert!(matches!(
            f.resolve("to-secrets/key"),
            Err(Error::Invalid(_))
        ));
        assert_eq!(paths(f.resolve("to-app").unwrap()), [("app".into(), true)]);
        assert_eq!(
            paths(f.resolve("abs").unwrap()),
            [("app/main.js".into(), false)]
        );
        assert_eq!(paths(f.resolve("up").unwrap()), [("app".into(), true)]);
        assert_eq!(
            paths(f.resolve("via").unwrap()),
            [("app/main.js".into(), false)]
        );
        assert_eq!(
            paths(f.resolve("to-app/main.js").unwrap()),
            [("app/main.js".into(), false)]
        );
        // The copy is named after the source as written.
        assert_eq!(f.resolve("via").unwrap()[0].named, PathBuf::from("via"));
        let e = f.resolve("loop-a").unwrap_err().to_string();
        assert!(e.contains("too many levels"), "{e}");
    }
}
