//! Files without a name, for a store that keeps nothing a `RUN` can reach
//! (kaniko mode on `/`, ADR-0051).
//!
//! There a `RUN` runs as root on ranikust's own `/`, in its PID namespace:
//! it can read any file that has a path. An anonymous file has none: an
//! `O_TMPFILE` inode in the state directory, or a `memfd` where the
//! filesystem has no `O_TMPFILE`: the overlayfs root of a Docker container
//! on kernel 6.8 refuses it [measured, VM `secrets` suite], so blobs sit in
//! memory there unless a volume is mounted at the state directory
//! [open: Q-340].
//! Never a named file unlinked later, which leaves a window. ranikust keeps
//! each one open for the life of the store and names it to its own code as
//! `/proc/self/fd/<n>`, which only ranikust's process resolves to that
//! file; ranikust is not dumpable, so `/proc/<ranikust>/fd` is closed to a
//! `RUN` without `CAP_SYS_PTRACE`.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use rustix::fs::{MemfdFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};

/// Where anonymous files are made, and the ones made so far.
#[derive(Debug)]
pub(crate) struct AnonFiles {
    /// The state directory, for `O_TMPFILE`.
    dir: OwnedFd,
    dir_path: PathBuf,
    /// `O_TMPFILE` was refused once: `memfd` from then on.
    memfd: AtomicBool,
    /// Every file made, kept open until the store goes: their numbers are
    /// the paths handed out.
    kept: Mutex<Vec<OwnedFd>>,
}

impl AnonFiles {
    /// Anonymous files in the directory `dir` (opened by the caller,
    /// `O_PATH | O_DIRECTORY`), named `dir_path` in messages.
    pub(crate) fn new(dir: OwnedFd, dir_path: &Path) -> Self {
        Self {
            dir,
            dir_path: dir_path.to_owned(),
            memfd: AtomicBool::new(false),
            kept: Mutex::new(Vec::new()),
        }
    }

    /// Whether files are `memfd`s (memory) rather than `O_TMPFILE` inodes.
    pub(crate) fn in_memory(&self) -> bool {
        self.memfd.load(Ordering::Relaxed)
    }

    /// A new, empty, anonymous file open for reading and writing, mode
    /// `0600`, close-on-exec. Not kept: [`AnonFiles::keep`] keeps it.
    pub(crate) fn create(&self) -> Result<File> {
        if !self.in_memory() {
            match rustix::fs::openat(
                &self.dir,
                ".",
                OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => return Ok(File::from(fd)),
                // No `O_TMPFILE` on this filesystem (EOPNOTSUPP), or in this
                // kernel (EISDIR: the flag reads as `O_DIRECTORY`).
                Err(Errno::OPNOTSUPP | Errno::ISDIR | Errno::INVAL) => {
                    if !self.memfd.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            target: "rnk_store",
                            "{}: no O_TMPFILE on this filesystem; state is kept in memory \
                             (memfd) instead",
                            self.dir_path.display()
                        );
                    }
                }
                Err(e) => return Err(self.err(e)),
            }
        }
        rustix::fs::memfd_create(c"ranikust-state", MemfdFlags::CLOEXEC)
            .map(File::from)
            .map_err(|e| self.err(e))
    }

    /// Keeps `file` open for the life of the store and returns its name for
    /// ranikust's own code: `/proc/self/fd/<n>`.
    pub(crate) fn keep(&self, file: File) -> PathBuf {
        let fd = OwnedFd::from(file);
        let path = PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()));
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(fd);
        path
    }

    fn err(&self, e: Errno) -> Error {
        Error::AnonymousFile {
            path: self.dir_path.clone(),
            source: e.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Seek as _, Write as _};

    use super::*;

    fn files(dir: &Path) -> AnonFiles {
        let fd = rustix::fs::open(
            dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        AnonFiles::new(fd, dir)
    }

    #[test]
    fn an_anonymous_file_has_no_name_and_reads_back_through_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        let mut f = anon.create().unwrap();
        f.write_all(b"state").unwrap();
        f.rewind().unwrap();
        let path = anon.keep(f);
        assert!(path.starts_with("/proc/self/fd"));
        let mut back = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut back)
            .unwrap();
        assert_eq!(back, "state");
        // Nothing appears in the directory.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn memfd_is_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.memfd.store(true, Ordering::Relaxed);
        let mut f = anon.create().unwrap();
        f.write_all(b"x").unwrap();
        let path = anon.keep(f);
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        assert!(anon.in_memory());
    }
}
