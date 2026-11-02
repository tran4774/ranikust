//! The stop signals of a build in kaniko mode on `/` (ADR-0077).
//!
//! A `Direct` `RUN` shares ranikust's PID namespace, so it can signal
//! ranikust. As the container's PID 1, ranikust gets only the signals it
//! handles, and it handles `SIGINT` and `SIGTERM` to stop a build. A `RUN`
//! sending those twice would stop the build before the store empties the
//! state directory. So they are blocked in every thread and read through a
//! `signalfd`, which gives the sender, and only the operator's are obeyed.
//!
//! Who counts as the operator:
//! - the kernel (a terminal's interrupt);
//! - a process outside the PID namespace (`docker stop`, a kubelet), which
//!   the kernel shows as pid 0;
//! - ranikust's parent (a CI shell, `docker run --init`).
//!
//! A signal queued with `rt_sigqueueinfo` names a sender its caller chose,
//! so only `kill` and `tgkill` count, whose sender the kernel sets.
//!
//! Blocked signals are queued for PID 1 too: blocking does not make
//! `docker stop` lost. `RUN` children get an empty mask back before
//! `execve` (`reset_process_state`). This closes nothing where ranikust is
//! not PID 1: a `RUN` can then `SIGKILL` it.

use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

/// `SIGINT` and `SIGTERM`, blocked in every thread, read from a `signalfd`.
#[derive(Debug)]
pub struct StopSignals {
    fd: OwnedFd,
}

/// A stop signal and whether the build obeys it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopSignal {
    /// `SIGINT` or `SIGTERM`.
    pub signo: i32,
    /// The sender's pid in ranikust's PID namespace; 0 for the kernel or a
    /// process outside it.
    pub pid: u32,
    /// Whether it comes from the operator (see the module).
    pub obeyed: bool,
}

fn stop_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` and `sigaddset` fill a set they own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        set
    }
}

/// Blocks `SIGINT` and `SIGTERM` in the calling thread, and so in every
/// thread it starts afterwards, and opens a `signalfd` for them.
///
/// # Errors
///
/// When the process already has more than one thread (a thread started
/// before would still take the signals), or a call fails.
pub fn block_stop_signals() -> io::Result<StopSignals> {
    let threads = std::fs::read_dir("/proc/self/task")?.count();
    if threads != 1 {
        return Err(io::Error::other(format!(
            "the stop signals must be blocked while the process has one thread, not {threads}"
        )));
    }
    let set = stop_set();
    // SAFETY: plain calls on a set this function owns.
    let fd = unsafe {
        let r = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        let fd = libc::signalfd(-1, &raw const set, libc::SFD_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(fd)
    };
    Ok(StopSignals { fd })
}

impl StopSignals {
    /// Waits for the next stop signal.
    ///
    /// # Errors
    ///
    /// When the read fails.
    pub fn wait(&self) -> io::Result<StopSignal> {
        // SAFETY: a zeroed `signalfd_siginfo` is valid, and `read` fills at
        // most its size.
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::signalfd_siginfo>();
        loop {
            // SAFETY: `info` is `size` bytes the call may write.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), (&raw mut info).cast(), size) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if usize::try_from(n).ok() != Some(size) {
                return Err(io::Error::other("short read from the signalfd"));
            }
            #[expect(clippy::cast_possible_wrap, reason = "signal numbers are small")]
            let signo = info.ssi_signo as i32;
            // SAFETY: `getppid` has no preconditions.
            let parent = unsafe { libc::getppid() };
            let parent = u32::try_from(parent).unwrap_or(0);
            return Ok(StopSignal {
                signo,
                pid: info.ssi_pid,
                obeyed: from_operator(info.ssi_code, info.ssi_pid, parent),
            });
        }
    }
}

/// Whether a stop signal with this `si_code` and sender comes from the
/// operator, ranikust's parent being `parent` (0 as PID 1).
pub(crate) fn from_operator(code: i32, pid: u32, parent: u32) -> bool {
    match code {
        libc::SI_KERNEL => true,
        libc::SI_USER | libc::SI_TKILL => pid == 0 || (parent != 0 && pid == parent),
        // `rt_sigqueueinfo` and the like: the sender is the caller's word.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::from_operator;

    #[test]
    fn only_the_operators_stop_signals_are_obeyed() {
        // As PID 1: the parent is 0.
        assert!(from_operator(libc::SI_USER, 0, 0), "docker stop");
        assert!(from_operator(libc::SI_KERNEL, 0, 0), "a terminal");
        assert!(!from_operator(libc::SI_USER, 42, 0), "a RUN's kill");
        assert!(!from_operator(libc::SI_TKILL, 42, 0), "a RUN's tgkill");
        // Started by a shell, pid 7.
        assert!(from_operator(libc::SI_USER, 7, 7), "the parent");
        assert!(!from_operator(libc::SI_USER, 8, 7), "anyone else");
        // A queued signal names whatever sender its caller chose.
        assert!(!from_operator(libc::SI_QUEUE, 0, 0));
        assert!(!from_operator(libc::SI_QUEUE, 7, 7));
    }
}
