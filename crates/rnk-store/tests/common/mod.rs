//! Helpers the store's red teams share.

use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use rustix::fs::{CWD, FileType, Mode, OFlags, inotify, mknodat};

/// Runs `f` on a thread of its own, since a FIFO opened by mistake blocks.
pub(crate) fn unblocked<T: Send + 'static>(
    case: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::channel();
    // The receiver is gone only once the case has failed as blocked.
    let worker = thread::spawn(move || drop(tx.send(f())));
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => panic!("{case}: blocked"),
        Err(RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(worker.join().unwrap_err())
        }
    }
}

/// An inotify watch for opens of some paths; `O_PATH`, which looks without opening, is not seen,
/// nor is an open that fails, as a socket's or a `0:0` whiteout's does with `ENXIO`.
pub(crate) struct Opens(OwnedFd);

impl Opens {
    pub(crate) fn watch(paths: &[&Path]) -> Self {
        let fd =
            inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK).unwrap();
        for p in paths {
            inotify::add_watch(&fd, *p, inotify::WatchFlags::OPEN).unwrap();
        }
        Self(fd)
    }

    pub(crate) fn seen(&self) -> bool {
        let mut buf = [0u8; 256];
        match rustix::io::read(&self.0, &mut buf) {
            Ok(n) => n > 0,
            Err(rustix::io::Errno::AGAIN) => false,
            Err(e) => panic!("read the inotify events: {e}"),
        }
    }

    /// The control: an open of a FIFO in `dir` is seen.
    pub(crate) fn control(dir: &Path) {
        let fifo = dir.join("control-fifo");
        mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
        let opens = Self::watch(&[&fifo]);
        drop(rustix::fs::open(&fifo, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty()).unwrap());
        assert!(opens.seen(), "the control: an open is seen");
        fs::remove_file(&fifo).unwrap();
    }
}
