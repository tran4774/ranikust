//! `flock` on lock files in the state directory.
//!
//! A lock belongs to the open file description, so every lock is taken on a
//! descriptor of its own: two threads of one process, each with its own
//! descriptor, exclude each other like two processes do. Descriptors are
//! `O_CLOEXEC` and never held across a fork; the kernel releases the lock
//! when the last one closes, even on `SIGKILL`.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rnk_rootfs::{OpenHow, RootPath};
use rustix::fs::FlockOperation;
use rustix::io::Errno;

/// Opens (creating it if missing) the lock file `rel` of the state
/// directory. A symlink there is refused, not followed.
pub(crate) fn open(state: &RootPath, rel: &Path) -> rnk_rootfs::Result<OwnedFd> {
    state.open_at(rel, &OpenHow::read().create(0o600).no_follow())
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

/// Takes the exclusive lock, waiting for it.
pub(crate) fn lock(fd: impl AsFd) -> io::Result<()> {
    loop {
        match rustix::fs::flock(fd.as_fd(), FlockOperation::LockExclusive) {
            Err(Errno::INTR) => {}
            r => return r.map_err(Into::into),
        }
    }
}
