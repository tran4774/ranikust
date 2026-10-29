//! Lexical helpers for paths inside a root.
//!
//! Nothing here touches the filesystem or resolves symlinks: these helpers
//! only shape the string that `openat2` resolves, or check tar names before
//! anything is created. The kernel does the actual, confined resolution.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// The path handed to `openat2`: relative, without root or `.` components,
/// `..` kept (the kernel clamps it at the root). `.` for the root itself.
pub(crate) fn relative(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => out.push(".."),
            Component::Normal(n) => out.push(n),
            // Never produced on Unix; kept as a name rather than dropped.
            Component::Prefix(p) => out.push(p.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// What a path names, for the operations that work on a directory entry.
pub(crate) enum Target<'a> {
    /// The root itself (`""`, `"/"`, `"."`).
    Root,
    /// An entry `name` in the directory `parent` (relative, as [`relative`]).
    Entry { parent: PathBuf, name: &'a OsStr },
}

/// Splits `p` into its parent directory and last name.
///
/// A path ending in `..` names no entry and is refused; so is any path whose
/// last component is not a plain name.
pub(crate) fn target(p: &Path) -> Result<Target<'_>> {
    let mut comps = p.components();
    let last = loop {
        match comps.next_back() {
            None => return Ok(Target::Root),
            Some(Component::CurDir) => {}
            Some(c) => break c,
        }
    };
    match last {
        Component::Normal(name) => Ok(Target::Entry {
            parent: relative(comps.as_path()),
            name,
        }),
        Component::RootDir => Ok(Target::Root),
        Component::ParentDir | Component::CurDir | Component::Prefix(_) => {
            Err(Error::InvalidPath {
                path: p.to_owned(),
                reason: "the last component must be a file name",
            })
        }
    }
}

/// Like [`target`], but the root itself is refused.
pub(crate) fn entry(p: &Path) -> Result<(PathBuf, &OsStr)> {
    match target(p)? {
        Target::Entry { parent, name } => Ok((parent, name)),
        Target::Root => Err(Error::InvalidPath {
            path: p.to_owned(),
            reason: "names the root itself, not an entry in it",
        }),
    }
}

/// Lexically cleans a path from untrusted input (a tar name, an ignore-list
/// entry): drops the root and `.` components and folds `..` into its parent.
///
/// Returns `None` when a `..` would climb above the root; such a name is
/// refused outright rather than clamped, as Docker refuses a tar "breakout".
/// The empty path means the root itself.
pub(crate) fn clean(p: &Path) -> Option<PathBuf> {
    let mut parts: Vec<&OsStr> = Vec::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(n) => parts.push(n),
            Component::Prefix(p) => parts.push(p.as_os_str()),
        }
    }
    Some(parts.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_strips_root_and_dots() {
        assert_eq!(relative(Path::new("/etc/passwd")), Path::new("etc/passwd"));
        assert_eq!(relative(Path::new("./a/./b/")), Path::new("a/b"));
        assert_eq!(relative(Path::new("/../a")), Path::new("../a"));
        assert_eq!(relative(Path::new("")), Path::new("."));
        assert_eq!(relative(Path::new("/")), Path::new("."));
        assert_eq!(relative(Path::new("//")), Path::new("."));
    }

    #[test]
    fn target_splits_last_name() {
        let Target::Entry { parent, name } = target(Path::new("/a/b/c")).unwrap() else {
            panic!("expected an entry");
        };
        assert_eq!(parent, Path::new("a/b"));
        assert_eq!(name, "c");

        let Target::Entry { parent, name } = target(Path::new("c/.")).unwrap() else {
            panic!("expected an entry");
        };
        assert_eq!(parent, Path::new("."));
        assert_eq!(name, "c");

        let Target::Entry { parent, .. } = target(Path::new("../../x")).unwrap() else {
            panic!("expected an entry");
        };
        assert_eq!(parent, Path::new("../.."));
    }

    #[test]
    fn target_root_and_dotdot() {
        for root in ["", "/", ".", "//", "./"] {
            assert!(
                matches!(target(Path::new(root)).unwrap(), Target::Root),
                "{root:?}"
            );
        }
        assert!(target(Path::new("a/..")).is_err());
        assert!(target(Path::new("..")).is_err());
        assert!(entry(Path::new("/")).is_err());
    }

    #[test]
    fn clean_folds_and_refuses_escapes() {
        assert_eq!(clean(Path::new("./a/b/../c")).unwrap(), Path::new("a/c"));
        assert_eq!(clean(Path::new("/a//b/")).unwrap(), Path::new("a/b"));
        assert_eq!(clean(Path::new("./")).unwrap(), Path::new(""));
        assert_eq!(clean(Path::new("a/..")).unwrap(), Path::new(""));
        assert!(clean(Path::new("../etc/passwd")).is_none());
        assert!(clean(Path::new("a/../../etc")).is_none());
        assert!(clean(Path::new("/..")).is_none());
    }
}
