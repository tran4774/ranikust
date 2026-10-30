//! `flock` on lock files in the state directory.
//!
//! A lock belongs to the open file description, so every lock is taken on a
//! descriptor of its own: two threads of one process, each with its own
//! descriptor, exclude each other like two processes do. Descriptors are
//! `O_CLOEXEC` and never held across a fork; the kernel releases the lock
//! when the last one closes, even on `SIGKILL`.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};

use rnk_rootfs::{FileType, OpenHow, RootPath};
use rustix::fs::FlockOperation;
use rustix::io::Errno;

/// Opens (creating it if missing) the lock file `rel` of the state
/// directory.
///
/// Anything but a regular file there is refused before it is opened for
/// I/O: a planted FIFO would block every build in `open`, and a device node
/// would be opened as root. A new file is created with `O_EXCL`, which
/// opens nothing that exists; an existing one must be a regular file, not a
/// symlink, and is opened by `RootPath::open_regular`, which pins the inode
/// with `O_PATH` and checks its type before reopening it.
pub(crate) fn open(state: &RootPath, rel: &Path) -> rnk_rootfs::Result<OwnedFd> {
    match state.open_at(rel, &OpenHow::read().create_new(0o600)) {
        Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {}
        r => return r,
    }
    if state.statx(rel)?.file_type() != FileType::Regular {
        return Err(rnk_rootfs::Error::InvalidPath {
            path: rel.to_owned(),
            reason: "a lock file must be a regular file",
        });
    }
    state.open_regular(rel)
}

/// Takes the exclusive lock without waiting: `false` when another
/// descriptor holds it.
pub(crate) fn try_lock(fd: impl AsFd) -> io::Result<bool> {
    loop {
        match rustix::fs::flock(fd.as_fd(), FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(true),
            Err(Errno::WOULDBLOCK) => return Ok(false),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// Takes the shared lock without waiting: `false` when another descriptor
/// holds the exclusive one.
pub(crate) fn try_lock_shared(fd: impl AsFd) -> io::Result<bool> {
    loop {
        match rustix::fs::flock(fd.as_fd(), FlockOperation::NonBlockingLockShared) {
            Ok(()) => return Ok(true),
            Err(Errno::WOULDBLOCK) => return Ok(false),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The per-digest locks this process holds, kept apart from `flock`: on
/// NFS, CIFS and some FUSE filesystems `flock` is emulated with per-process
/// record locks, so two threads of one process would not exclude each
/// other there.
#[derive(Debug, Default)]
pub(crate) struct InProcess {
    held: Mutex<HashSet<PathBuf>>,
    freed: Condvar,
}

impl InProcess {
    /// Waits until no other thread holds `key`, then holds it until the
    /// guard drops; also returns whether it had to wait.
    pub(crate) fn acquire(&self, key: &Path) -> (Held<'_>, bool) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let mut waited = false;
        while held.contains(key) {
            waited = true;
            held = self
                .freed
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
        held.insert(key.to_owned());
        let guard = Held {
            set: self,
            key: key.to_owned(),
        };
        (guard, waited)
    }
}

/// A key held in [`InProcess`]; released on drop.
#[derive(Debug)]
pub(crate) struct Held<'a> {
    set: &'a InProcess,
    key: PathBuf,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.set
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
        self.set.freed.notify_all();
    }
}

/// Takes the exclusive lock, waiting for it.
pub(crate) fn lock(fd: impl AsFd) -> io::Result<()> {
    loop {
        match rustix::fs::flock(fd.as_fd(), FlockOperation::LockExclusive) {
            Err(Errno::INTR) => {}
            r => return r.map_err(Into::into),
        }
    }
}
