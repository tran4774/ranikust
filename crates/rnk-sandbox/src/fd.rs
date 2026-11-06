//! Descriptors that a `RUN`'s init or command must keep through the
//! child's `dup2` onto its standard streams.

use std::os::fd::{AsRawFd, OwnedFd};

/// `fd`, or a close-on-exec duplicate numbered 3 or higher when it is 0, 1
/// or 2 (ranikust started with a standard stream closed): the child's
/// `dup2` onto 0–2 must not overwrite it.
pub(crate) fn fd_above_stdio(fd: OwnedFd) -> rustix::io::Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    rustix::io::fcntl_dupfd_cloexec(&fd, 3)
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use rustix::fs::{Mode, OFlags};

    use super::*;

    /// A descriptor that took a closed standard stream's number moves above
    /// 0–2, so the child's `dup2` onto them cannot close it; one above them
    /// stays.
    #[test]
    fn a_descriptor_on_a_standard_stream_moves_above_it() {
        let high =
            rustix::fs::open("/dev/null", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).unwrap();
        let raw = high.as_raw_fd();
        assert!(raw >= 3);
        assert_eq!(fd_above_stdio(high).unwrap().as_raw_fd(), raw);
        // SAFETY: the child only makes syscalls on stack values and exits
        // with `_exit`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: as above; `0` is the child's own, closed first, and
            // `fd` was just opened and nothing else owns it.
            unsafe {
                libc::close(0);
                let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if fd != 0 {
                    libc::_exit(2);
                }
                let Ok(moved) = fd_above_stdio(OwnedFd::from_raw_fd(fd)) else {
                    libc::_exit(3)
                };
                let closed = libc::fcntl(0, libc::F_GETFD) == -1;
                libc::_exit(if moved.as_raw_fd() >= 3 && closed {
                    0
                } else {
                    1
                });
            }
        }
        let mut status = 0;
        // SAFETY: reaping the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "{status:#x}"
        );
    }
}
