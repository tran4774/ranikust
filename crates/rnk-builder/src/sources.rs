//! Which files a `COPY` or `ADD` source names: in the build context,
//! filtered by `.dockerignore`, or in another stage's root.
//!
//! [open: Q-080] Interim, the card's recommendation (a): this crate
//! resolves sources, against the context walk's entries (so an ignored file
//! is never named or read), with BuildKit's wildcard rules ([`crate::glob`]).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rnk_context::Context;
use rnk_ignore::Matcher;
use rnk_rootfs::{FileType, OpenHow, RootPath};

use crate::error::{Error, Result};
use crate::glob;

/// The context's entries that `.dockerignore` keeps, with whether each is a
/// directory; walked once per build.
#[derive(Debug, Default)]
pub struct ContextFiles {
    entries: BTreeMap<PathBuf, bool>,
}

impl ContextFiles {
    /// Walks `context` with `ignore`.
    ///
    /// # Errors
    ///
    /// [`Error::Context`] when the walk fails.
    pub fn load(context: &Context, ignore: Option<&Matcher>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for entry in context.walk(ignore) {
            let entry = entry?;
            // The directories leading to a kept entry are kept too, as
            // BuildKit's filtered walk reports them.
            for a in entry.path.ancestors().skip(1) {
                if !a.as_os_str().is_empty() {
                    entries.insert(a.to_owned(), true);
                }
            }
            entries.insert(entry.path, entry.meta.file_type == FileType::Directory);
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
    /// # Errors
    ///
    /// [`Error::Invalid`] when nothing kept matches, or the pattern is
    /// malformed.
    pub(crate) fn resolve(&self, source: &str) -> Result<Vec<(PathBuf, bool)>> {
        let clean = clean_source(source);
        if clean.as_os_str().is_empty() {
            return Ok(vec![(clean, true)]);
        }
        let text = clean.to_string_lossy();
        if !glob::has_wildcard(&text) {
            return match self.entries.get(&clean) {
                Some(&dir) => Ok(vec![(clean, dir)]),
                None => Err(not_found(source)),
            };
        }
        let parts: Vec<&str> = text.split('/').collect();
        let mut out = Vec::new();
        for (path, &dir) in &self.entries {
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
                out.push((path.clone(), dir));
            }
        }
        if out.is_empty() {
            return Err(not_found(source));
        }
        Ok(out)
    }
}

/// The entries of `root` (a stage's or an image's) that `source` names,
/// sorted, with whether each is a directory. A plain path is returned as
/// it is, resolved when copied; a wildcard pattern is matched one directory
/// level at a time, listing directories through `root`.
///
/// # Errors
///
/// [`Error::Invalid`] when nothing matches or the pattern is malformed;
/// [`Error::Rootfs`] when a directory cannot be listed.
pub(crate) fn resolve_in_root(root: &RootPath, source: &str) -> Result<Vec<(PathBuf, bool)>> {
    let clean = clean_source(source);
    let text = clean.to_string_lossy().into_owned();
    if !glob::has_wildcard(&text) {
        let dir = match root.open_at(&clean, &OpenHow::path().directory()) {
            Ok(_) => true,
            Err(e) if e.raw_os_error() == Some(libc_enotdir()) => false,
            Err(e) if e.raw_os_error() == Some(libc_enoent()) => return Err(not_found(source)),
            Err(e) => return Err(e.into()),
        };
        return Ok(vec![(clean, dir)]);
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
            // Only directories lead further.
            next.retain(|p| root.open_at(p, &OpenHow::path().directory()).is_ok());
        }
        found = next;
    }
    let mut out = Vec::new();
    for p in found {
        match root.open_at(&p, &OpenHow::path().directory()) {
            Ok(_) => out.push((p, true)),
            Err(e) if e.raw_os_error() == Some(libc_enotdir()) => out.push((p, false)),
            // A literal last component that does not exist.
            Err(e) if e.raw_os_error() == Some(libc_enoent()) => {}
            Err(e) => return Err(e.into()),
        }
    }
    out.sort();
    if out.is_empty() {
        return Err(not_found(source));
    }
    Ok(out)
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

    fn files(paths: &[(&str, bool)]) -> ContextFiles {
        ContextFiles {
            entries: paths.iter().map(|(p, d)| (PathBuf::from(p), *d)).collect(),
        }
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
            ("a.js", false),
            ("b.js", false),
            ("src", true),
            ("src/c.js", false),
        ]);
        assert_eq!(
            f.resolve("./a.js").unwrap(),
            vec![(PathBuf::from("a.js"), false)]
        );
        assert_eq!(
            f.resolve("*.js").unwrap(),
            vec![
                (PathBuf::from("a.js"), false),
                (PathBuf::from("b.js"), false)
            ]
        );
        // A wildcard matches at its own depth only.
        assert_eq!(
            f.resolve("*/c.js").unwrap(),
            vec![(PathBuf::from("src/c.js"), false)]
        );
        assert_eq!(f.resolve(".").unwrap(), vec![(PathBuf::new(), true)]);
        assert!(matches!(f.resolve("missing"), Err(Error::Invalid(_))));
        assert!(matches!(f.resolve("*.rs"), Err(Error::Invalid(_))));
        assert!(matches!(f.resolve("[a"), Err(Error::Invalid(_))));
    }
}
