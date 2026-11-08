//! Shared helpers for the unit tests.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// Every entry below `dir`, `dir` included, never following a symlink nor
/// opening anything but a regular file: one line each, whose fields split
/// at spaces are 0 the path, 1 the mode with the type, 2 `uid:gid`, 3 the
/// link count, 4 the size, 5 the mtime and 6 the ctime as `sec.nsec`, then
/// the content (a file's bytes, a symlink's target). The times show a
/// change made and undone, such as a mode set and set back.
pub(crate) fn snapshot(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let m = fs::symlink_metadata(&p).unwrap();
        let ft = m.file_type();
        let content = if ft.is_file() {
            String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned()
        } else if ft.is_symlink() {
            fs::read_link(&p).unwrap().display().to_string()
        } else {
            String::new()
        };
        out.push(format!(
            "{} {:o} {}:{} {} {} {}.{} {}.{} {content}",
            p.display(),
            m.mode(),
            m.uid(),
            m.gid(),
            m.nlink(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec()
        ));
        if ft.is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
        }
    }
    out.sort();
    out
}

/// A fresh directory for a test's state directories, root's, mode
/// `0755`, below `/var/lib`, whose parents root alone owns and writes
/// (a temporary directory is below a sticky `/tmp`, which `--userns`
/// refuses); removed after.
pub(crate) struct Base(pub(crate) PathBuf);

impl Base {
    pub(crate) fn new(what: &str) -> Self {
        let p = PathBuf::from(format!(
            "/var/lib/rnk-red-team-{what}-{}",
            std::process::id()
        ));
        // What an earlier run of this process id left, if anything.
        let _ = fs::remove_dir_all(&p);
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        Self(p)
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        // A failed removal leaves only a test's own directory.
        let _ = fs::remove_dir_all(&self.0);
    }
}
