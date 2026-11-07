//! A process left outside the build's user namespace, for a fallback
//! (ADR-0136).
//!
//! The build's user namespace cannot be left once entered, and kaniko mode
//! must never run in it: there it would act on the container's `/` as the
//! map's root. So where a set-up that fails in it may still fall back to
//! kaniko mode, the process forks first ([`fork_to_wait`]): the child
//! enters the namespace, sets up and builds; the parent stays as it was and
//! waits ([`Waiting::wait`]). Only before its build may the child ask for
//! the fallback ([`Supervised::fall_back`]); otherwise the parent ends as
//! the child ended, its exit code or its signal.
//!
//! `SIGINT` and `SIGTERM` reach the build once each: the parent takes them,
//! blocked and read with `sigwaitinfo`, whether a terminal sent them to the
//! process group or a supervisor to ranikust alone, and sends the child
//! [`FORWARDED_STOP`] for each; the child ignores the two itself. Until the
//! child handles that signal, its default action ends the child, and the
//! parent then ends by the stop signal it got. The child gets `SIGKILL`
//! when the parent dies (`PR_SET_PDEATHSIG`); any other signal acts on the
//! parent as it would have without the fork.

use std::io::{self, Write as _};
use std::os::fd::OwnedFd;

use rustix::process::Signal;

/// The signal the parent sends the child for each `SIGINT` or `SIGTERM`
/// it gets: `SIGUSR1`. The child's build handles it as it would those two.
pub const FORWARDED_STOP: i32 = libc::SIGUSR1;

/// The child's exit status when it asks for the fallback: an error's, so
/// that a parent that cannot read the reason ends as for one.
const FALLBACK_STATUS: i32 = 1;

/// The longest reason the child sends: less than `PIPE_BUF`, so that it is
/// written at once.
const MAX_REASON: usize = 4000;

/// [`fork_to_wait`] in the child: it may enter the build's user namespace.
#[derive(Debug)]
#[must_use = "the child either proceeds to its build or falls back"]
pub struct Supervised {
    /// The pipe's end the reason goes to; close-on-exec.
    reason: OwnedFd,
    /// The parent's pid, which `getppid` gives while it lives.
    parent: i32,
}

/// [`fork_to_wait`] in the parent, as it was before the fork but for its
/// signal mask and `SIGCHLD`'s disposition, until [`Waiting::wait`].
#[must_use = "the child runs unwatched until it is waited for"]
pub struct Waiting {
    child: i32,
    /// The pipe's end the reason comes from; non-blocking.
    reason: OwnedFd,
    /// The signal mask before the fork.
    mask: libc::sigset_t,
    /// `SIGCHLD`'s disposition before the fork.
    sigchld: libc::sigaction,
}

impl std::fmt::Debug for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waiting")
            .field("child", &self.child)
            .finish_non_exhaustive()
    }
}

/// What [`fork_to_wait`] returns, in each process.
#[derive(Debug)]
pub enum Forked {
    /// In the child.
    Child(Supervised),
    /// In the parent.
    Parent(Box<Waiting>),
}

/// The stop signals the parent forwards.
const STOPS: [i32; 2] = [libc::SIGINT, libc::SIGTERM];

/// What the parent waits for: the stop signals and `SIGCHLD`.
const WAITED: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGCHLD];

/// The set of `sigs`.
fn signal_set(sigs: &[i32]) -> libc::sigset_t {
    // SAFETY: `sigemptyset` and `sigaddset` fill a set this function owns.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        for &sig in sigs {
            libc::sigaddset(&raw mut set, sig);
        }
        set
    }
}

/// Sets the disposition of `sig` to `handler` (`SIG_DFL`, `SIG_IGN`) and
/// returns the one before.
fn set_disposition(sig: i32, handler: libc::sighandler_t) -> io::Result<libc::sigaction> {
    // SAFETY: a zeroed `sigaction` is valid (no handler, no flags, an empty
    // mask); `sigaction` reads `new` and writes `old`, both alive across it.
    unsafe {
        let mut new: libc::sigaction = std::mem::zeroed();
        new.sa_sigaction = handler;
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(sig, &raw const new, &raw mut old) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(old)
    }
}

/// Sets the signal mask of the calling thread, `how` as `pthread_sigmask`.
fn set_mask(how: i32, set: &libc::sigset_t) -> io::Result<libc::sigset_t> {
    // SAFETY: a zeroed `sigset_t` is a valid set to write; the call reads
    // `set` and writes `old`, both alive across it.
    unsafe {
        let mut old: libc::sigset_t = std::mem::zeroed();
        match libc::pthread_sigmask(how, set, &raw mut old) {
            0 => Ok(old),
            e => Err(io::Error::from_raw_os_error(e)),
        }
    }
}

/// Forks, so that the child may enter the build's user namespace while the
/// parent stays outside it, to fall back to another mode if the child asks
/// ([`Supervised::fall_back`]). Before the fork, `SIGINT`, `SIGTERM` and
/// `SIGCHLD` are blocked and `SIGCHLD` gets its default disposition (an
/// ignored one would reap the child unseen); stdout is flushed, so that
/// nothing buffered is written twice. The child gets the mask back,
/// ignores `SIGINT` and `SIGTERM` (each comes forwarded, as
/// [`FORWARDED_STOP`]), and `SIGKILL` when the parent dies; a step of that
/// which fails asks for the fallback. Call while single-threaded.
///
/// # Errors
///
/// More than one thread, or a pipe, the mask, the disposition or the fork
/// failing, with the process as before the call.
pub fn fork_to_wait() -> io::Result<Forked> {
    crate::setup::require_single_thread().map_err(|e| io::Error::other(e.to_string()))?;
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK)?;
    let _ = io::stdout().flush();
    let parent = rustix::process::getpid().as_raw_nonzero().get();
    let mask = set_mask(libc::SIG_BLOCK, &signal_set(&WAITED))?;
    let sigchld = match set_disposition(libc::SIGCHLD, libc::SIG_DFL) {
        Ok(old) => old,
        Err(e) => {
            let _ = set_mask(libc::SIG_SETMASK, &mask);
            return Err(e);
        }
    };
    // SAFETY: the process is single-threaded (checked above), so the child,
    // a copy of this one thread, holds no lock another thread took.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        restore(&mask, &sigchld);
        return Err(e);
    }
    if pid == 0 {
        drop(read);
        let child = Supervised {
            reason: write,
            parent,
        };
        if let Err(e) = child_signals(&mask, parent) {
            child.fall_back(&format!(
                "the process that builds in the build's user namespace cannot set its signals: {e}"
            ));
        }
        return Ok(Forked::Child(child));
    }
    drop(write);
    Ok(Forked::Parent(Box::new(Waiting {
        child: pid,
        reason: read,
        mask,
        sigchld,
    })))
}

/// The child's signals: `SIGINT` and `SIGTERM` ignored, the mask `mask`
/// back, `SIGKILL` when the parent `parent` dies, and the parent still
/// alive once that is armed.
fn child_signals(mask: &libc::sigset_t, parent: i32) -> io::Result<()> {
    set_disposition(libc::SIGINT, libc::SIG_IGN)?;
    set_disposition(libc::SIGTERM, libc::SIG_IGN)?;
    set_mask(libc::SIG_SETMASK, mask)?;
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
    check_parent(parent)
}

/// `Ok` while `parent` is this process's parent: once the death signal is
/// armed, it then comes when the parent dies.
fn check_parent(parent: i32) -> io::Result<()> {
    let now = rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get());
    if now == parent {
        Ok(())
    } else {
        Err(io::Error::other(
            "the process that waits outside the build's user namespace is gone",
        ))
    }
}

/// The parent's mask and `SIGCHLD`'s disposition as before the fork.
fn restore(mask: &libc::sigset_t, sigchld: &libc::sigaction) {
    let _ = set_mask(libc::SIG_SETMASK, mask);
    // SAFETY: `sigaction` reads `sigchld`, alive across the call.
    unsafe {
        libc::sigaction(libc::SIGCHLD, sigchld, std::ptr::null_mut());
    }
}

impl Supervised {
    /// The set-up is done and the build starts: closes the way back, so
    /// that the parent ends as this process does whatever follows. Arms
    /// the death signal again (a change of credentials clears it) and
    /// checks the parent lives.
    ///
    /// # Errors
    ///
    /// The parent is gone, or the death signal cannot be armed.
    pub fn proceed(self) -> io::Result<()> {
        drop(self.reason);
        if rustix::process::parent_process_death_signal()? != Some(Signal::KILL) {
            rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
        }
        check_parent(self.parent)
    }

    /// Asks the parent to fall back, for `reason`, and exits: before the
    /// build, once what the set-up made is undone. The reason is cut to
    /// [`MAX_REASON`] bytes.
    pub fn fall_back(self, reason: &str) -> ! {
        let mut end = reason.len().min(MAX_REASON);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        let _ = rustix::io::write(&self.reason, &reason.as_bytes()[..end]);
        std::process::exit(FALLBACK_STATUS)
    }
}

/// How the child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// With this exit code.
    Exited(i32),
    /// By this signal.
    Killed(i32),
}

/// What the parent does once the child ended.
#[derive(Debug, PartialEq, Eq)]
enum Then {
    /// Fall back, for this reason.
    FallBack(String),
    /// Exit with this code.
    Exit(i32),
    /// End by this signal.
    Raise(i32),
}

/// What the parent does once the child ended as `end`, a stop signal
/// `stop` having come meanwhile, the child having sent `reason`: a stop
/// wins over the fallback, and stands for the forwarded signal that ended
/// the child; the fallback needs the reason and the child's status for it.
fn then(end: End, stop: Option<i32>, reason: Option<String>) -> Then {
    match (end, stop, reason) {
        (End::Exited(FALLBACK_STATUS), Some(sig), Some(_)) => Then::Raise(sig),
        (End::Killed(FORWARDED_STOP), Some(sig), _) => Then::Raise(sig),
        (End::Exited(FALLBACK_STATUS), None, Some(reason)) => Then::FallBack(reason),
        (End::Exited(code), _, _) => Then::Exit(code),
        (End::Killed(sig), _, _) => Then::Raise(sig),
    }
}

impl Waiting {
    /// Waits for the child, forwarding each `SIGINT` and `SIGTERM` as
    /// [`FORWARDED_STOP`] and reaping any other child it gets (as a
    /// container's PID 1). Returns only where the child asked for the
    /// fallback and no stop signal came, with the reason, the process's
    /// mask and `SIGCHLD`'s disposition as before the fork. Otherwise the
    /// process ends as the child did: its exit code, or its signal (the
    /// stop signal for the forwarded one), with the default action, and
    /// the shell's `128 + n` where that does not end it (PID 1).
    pub fn wait(self: Box<Self>) -> String {
        let waited = signal_set(&WAITED);
        let mut stop = None;
        let end = loop {
            // SAFETY: `sigwaitinfo` reads `waited`, alive across the call,
            // and takes no `siginfo_t`.
            let sig = unsafe { libc::sigwaitinfo(&raw const waited, std::ptr::null_mut()) };
            match sig {
                libc::SIGINT | libc::SIGTERM => {
                    stop.get_or_insert(sig);
                    // SAFETY: a signal to the child, which is not reaped yet.
                    unsafe {
                        libc::kill(self.child, FORWARDED_STOP);
                    }
                }
                libc::SIGCHLD => {
                    if let Some(end) = reap(self.child) {
                        break end;
                    }
                }
                _ if sig < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {
                }
                _ => break wait_blocking(self.child),
            }
        };
        // A stop that came with the child's end.
        let stops = signal_set(&STOPS);
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            // SAFETY: as `sigwaitinfo`, with a timeout alive across the call.
            let sig = unsafe {
                libc::sigtimedwait(&raw const stops, std::ptr::null_mut(), &raw const zero)
            };
            if sig <= 0 {
                break;
            }
            stop.get_or_insert(sig);
        }
        let reason = read_reason(&self.reason);
        match then(end, stop, reason) {
            Then::FallBack(reason) => {
                restore(&self.mask, &self.sigchld);
                reason
            }
            Then::Exit(code) => std::process::exit(code),
            Then::Raise(sig) => end_by(sig),
        }
    }
}

/// Reaps every child that ended; how `child` ended, if it did.
fn reap(child: i32) -> Option<End> {
    let mut found = None;
    loop {
        let mut status = 0;
        // SAFETY: `waitpid` writes the status it reports.
        let pid = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
        if pid <= 0 {
            return found;
        }
        if pid == child {
            found = end_of(status);
        }
    }
}

/// Waits for `child` alone, where waiting for signals failed.
fn wait_blocking(child: i32) -> End {
    loop {
        let mut status = 0;
        // SAFETY: `waitpid` writes the status it reports.
        let pid = unsafe { libc::waitpid(child, &raw mut status, 0) };
        if pid == child
            && let Some(end) = end_of(status)
        {
            return end;
        }
        if pid < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            // Reaped unseen: the status is lost, so an error's.
            return End::Exited(FALLBACK_STATUS + 1);
        }
    }
}

/// The end a `waitpid` status gives, if it is one.
fn end_of(status: i32) -> Option<End> {
    if libc::WIFEXITED(status) {
        Some(End::Exited(libc::WEXITSTATUS(status)))
    } else if libc::WIFSIGNALED(status) {
        Some(End::Killed(libc::WTERMSIG(status)))
    } else {
        None
    }
}

/// The reason the child sent, if any, read without waiting.
fn read_reason(fd: &OwnedFd) -> Option<String> {
    let mut buf = vec![0_u8; MAX_REASON];
    let mut len = 0;
    while len < buf.len() {
        match rustix::io::read(fd, &mut buf[len..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n,
        }
    }
    (len > 0).then(|| String::from_utf8_lossy(&buf[..len]).into_owned())
}

/// Ends the process by `sig`, with its default action, as the child ended;
/// with `128 + sig` where that does not end it: the kernel drops a signal a
/// PID 1 sends itself with its default action.
fn end_by(sig: i32) -> ! {
    let _ = set_disposition(sig, libc::SIG_DFL);
    let _ = set_mask(libc::SIG_UNBLOCK, &signal_set(&[sig]));
    // SAFETY: `raise` sends a signal to this thread.
    unsafe {
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child's end decides, but a stop signal wins over the fallback
    /// and stands for the forwarded signal; the fallback needs both the
    /// reason and its status.
    #[test]
    fn the_parent_ends_as_the_child_unless_it_falls_back_unstopped() {
        let why = || Some("why".to_owned());
        let int = libc::SIGINT;
        assert_eq!(
            then(End::Exited(FALLBACK_STATUS), None, why()),
            Then::FallBack("why".into())
        );
        assert_eq!(
            then(End::Exited(FALLBACK_STATUS), Some(int), why()),
            Then::Raise(int)
        );
        assert_eq!(
            then(End::Killed(FORWARDED_STOP), Some(int), None),
            Then::Raise(int)
        );
        assert_eq!(
            then(End::Killed(FORWARDED_STOP), None, None),
            Then::Raise(FORWARDED_STOP)
        );
        assert_eq!(
            then(End::Exited(FALLBACK_STATUS), None, None),
            Then::Exit(1)
        );
        assert_eq!(then(End::Exited(0), None, why()), Then::Exit(0));
        assert_eq!(then(End::Exited(130), Some(int), None), Then::Exit(130));
        assert_eq!(then(End::Exited(0), Some(int), None), Then::Exit(0));
        assert_eq!(
            then(End::Killed(libc::SIGKILL), None, None),
            Then::Raise(libc::SIGKILL)
        );
        assert_eq!(
            then(End::Killed(libc::SIGKILL), Some(int), why()),
            Then::Raise(libc::SIGKILL)
        );
    }
}
