//! Shared helpers for the unit tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::io::Errno;

use crate::{Error, RootPath};

/// A fresh temp dir holding `root/` (and room for siblings outside the
/// root); removed on drop, so also when a test fails.
pub(crate) struct Fixture {
    _tmp: tempfile::TempDir,
    pub(crate) host: PathBuf,
    pub(crate) root: RootPath,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // The temp dir may sit below a symlink, which `RootPath::open` refuses.
        let host = fs::canonicalize(tmp.path()).unwrap().join("root");
        fs::create_dir(&host).unwrap();
        let root = RootPath::open(&host).unwrap();
        Self {
            _tmp: tmp,
            host,
            root,
        }
    }

    /// The host path of `p` inside the root: for setting up and checking
    /// fixtures only, never for code under test.
    pub(crate) fn host(&self, p: &str) -> PathBuf {
        self.host.join(p)
    }

    /// A directory next to the root, outside it, with a few entries; and its
    /// listing, to check later that nothing there changed.
    pub(crate) fn outside(&self) -> (PathBuf, Snapshot) {
        let out = self.host.with_file_name("outside");
        fs::create_dir_all(out.join("sub")).unwrap();
        fs::write(out.join("sentinel"), b"x").unwrap();
        fs::write(out.join("sub/file"), b"y").unwrap();
        let t = snapshot(&out);
        (out, t)
    }

    /// The owner of the temp dir: the ids an unprivileged test may chown to.
    pub(crate) fn ids(&self) -> (u32, u32) {
        let md = fs::metadata(&self.host).unwrap();
        (md.uid(), md.gid())
    }
}

impl Drop for Fixture {
    /// Makes every directory writable again, so that the temp dir can be
    /// removed even when a test left read-only directories (also on failure).
    fn drop(&mut self) {
        let mut stack = vec![self._tmp.path().to_owned()];
        while let Some(dir) = stack.pop() {
            // Best effort: a failure only leaves the temp dir behind.
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
}

/// Every path below `dir`, relative to it.
pub(crate) fn tree(dir: &Path) -> BTreeSet<PathBuf> {
    snapshot(dir).into_keys().collect()
}

/// Paths with mode, size, mtime (seconds, nanoseconds) and link count.
pub(crate) type Snapshot = BTreeMap<PathBuf, (u32, u64, i64, i64, u64)>;

/// Every path below `dir`, relative to it, with mode, size, mtime and link
/// count: enough to see that nothing there changed.
pub(crate) fn snapshot(dir: &Path) -> Snapshot {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p.clone());
            }
            let rel = p.strip_prefix(dir).unwrap().to_owned();
            out.insert(
                rel,
                (md.mode(), md.len(), md.mtime(), md.mtime_nsec(), md.nlink()),
            );
        }
    }
    out
}

/// The errno of an error, if it has one.
pub(crate) fn errno(e: &Error) -> Option<Errno> {
    e.raw_os_error().map(Errno::from_raw_os_error)
}
