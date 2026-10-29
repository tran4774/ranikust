//! Shared helpers for the unit tests.

use std::fs;
use std::path::PathBuf;

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
}

/// The errno of an error, if it has one.
pub(crate) fn errno(e: &Error) -> Option<Errno> {
    e.raw_os_error().map(Errno::from_raw_os_error)
}
