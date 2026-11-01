//! The pipes a `RUN` writes its stdout and stderr to, and the thread that
//! copies them to ranikust's own streams (ADR-0057).
//!
//! No `RUN` ever holds ranikust's stream descriptors: one it inherited
//! would share their open file description, so a root `RUN` could
//! `ftruncate` or `pwrite` an operator's `> build.log`, or set `O_NONBLOCK`
//! on it. A `RUN` whose `USER` is not root also needs its streams to be its
//! own to reopen `/dev/stdout` (`/proc/self/fd/1`, kaniko#2928): runc gives
//! a container's streams to the container's user, which ranikust cannot do
//! with the operator's terminal or log file. So each stream the `RUN`
//! would inherit becomes a pipe, owned by the `RUN`'s ids, and one thread
//! copies every pipe to the matching stream of ranikust.
//!
//! The thread never waits for the end of a pipe to finish: a write end may
//! outlive the `RUN`'s processes (passed over a Unix socket, or opened
//! through `/proc/<pid>/fd` by another process of the same user). Once the
//! `RUN` is reaped and swept, [`Relay::finish`] signals the thread, which
//! reads what is left without blocking, gives the output streams a bounded
//! time, and exits.

use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};
use rustix::fs::OFlags;
use rustix::io::Errno;
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Uid};

use crate::child::ChildStdio;
use crate::error::{Result, sys_err};

/// How long, once the `RUN` is gone, the thread keeps trying to write what
/// it still holds to a stream that does not take it.
const DRAIN_TIME: Duration = Duration::from_secs(2);

/// The relay of one `RUN`: the thread and the event that stops it.
#[derive(Debug)]
pub(crate) struct Relay {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Relay {
    /// Replaces each of stdout and stderr in `stdio` that the child would
    /// inherit with the write end of a new pipe, kept in `keep` until the
    /// clone and owned by `uid`:`gid` when they are not root (an id this
    /// process cannot give leaves the pipe root's: the stream still stays
    /// the `RUN`'s own). `None` when no stream is inherited.
    pub(crate) fn start(
        stdio: &mut [ChildStdio; 3],
        keep: &mut Vec<OwnedFd>,
        uid: Uid,
        gid: Gid,
    ) -> Result<Option<Self>> {
        // ranikust's own stdout and stderr.
        Self::start_into(stdio, keep, uid, gid, [0, 1, 2])
    }

    /// [`Relay::start`], copying slot `i` to the descriptor `out[i]`.
    fn start_into(
        stdio: &mut [ChildStdio; 3],
        keep: &mut Vec<OwnedFd>,
        uid: Uid,
        gid: Gid,
        out: [RawFd; 3],
    ) -> Result<Option<Self>> {
        let mut streams = Vec::new();
        for slot in [1usize, 2] {
            if stdio[slot] != ChildStdio::Inherit {
                continue;
            }
            let (read, write) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
                .map_err(|e| sys_err("create a stdio pipe", e))?;
            let write = rustix::io::fcntl_dupfd_cloexec(&write, 3)
                .map_err(|e| sys_err("duplicate a stdio pipe", e))?;
            if !uid.is_root()
                && let Err(e) = rustix::fs::fchown(&write, Some(uid), Some(gid))
            {
                tracing::debug!("rnk-sandbox: RUN stream {slot} stays root's: fchown: {e}");
            }
            stdio[slot] = ChildStdio::Fd(std::os::fd::AsRawFd::as_raw_fd(&write));
            keep.push(write);
            streams.push(Stream {
                from: read,
                to: out[slot],
                open: true,
            });
        }
        if streams.is_empty() {
            return Ok(None);
        }
        let stop = rustix::event::eventfd(0, EventfdFlags::CLOEXEC)
            .map_err(|e| sys_err("create the stdio relay's event", e))?;
        let signal = rustix::io::fcntl_dupfd_cloexec(&stop, 3)
            .map_err(|e| sys_err("duplicate the stdio relay's event", e))?;
        let thread = std::thread::Builder::new()
            .name("rnk-sandbox-stdio".into())
            .spawn(move || relay(streams, &signal))
            .map_err(|e| sys_err("start the stdio relay", e))?;
        Ok(Some(Self {
            stop,
            thread: Some(thread),
        }))
    }

    /// Once the `RUN`'s processes are reaped and swept: copies what the
    /// pipes still hold, without waiting for their ends, and joins the
    /// thread.
    pub(crate) fn finish(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let _ = rustix::io::write(&self.stop, &1u64.to_ne_bytes());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

struct Stream {
    from: OwnedFd,
    to: RawFd,
    /// The output stream still takes writes.
    open: bool,
}

/// The thread: copies each pipe to its stream until its end, or until
/// `stop` is signalled, then drains.
fn relay(mut streams: Vec<Stream>, stop: &OwnedFd) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut deadline = None;
    while !streams.is_empty() {
        if deadline.is_none() {
            let mut fds: Vec<PollFd<'_>> = std::iter::once(PollFd::new(stop, PollFlags::IN))
                .chain(streams.iter().map(|s| PollFd::new(&s.from, PollFlags::IN)))
                .collect();
            match rustix::event::poll(&mut fds, None) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(_) => return,
            }
            if !fds[0].revents().is_empty() {
                deadline = Some(Instant::now() + DRAIN_TIME);
                for s in &streams {
                    let _ = set_nonblocking(&s.from);
                }
                continue;
            }
            let ready: Vec<bool> = fds[1..].iter().map(|f| !f.revents().is_empty()).collect();
            drop(fds);
            let mut ready = ready.into_iter();
            streams.retain_mut(|s| {
                if !ready.next().unwrap_or(false) {
                    return true;
                }
                copy_once(s, &mut buf, stop, None)
            });
        } else {
            // Draining: what is left, never blocking on a pipe, and never
            // past the deadline: a writer that outlived the `RUN` (the sweep
            // gave up) may keep filling the pipe forever.
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return;
            }
            streams.retain_mut(|s| copy_once(s, &mut buf, stop, deadline));
        }
    }
}

/// One read from the pipe, written out; false once the pipe has nothing
/// more to give (its end, an error, or, while draining, nothing left).
fn copy_once(s: &mut Stream, buf: &mut [u8], stop: &OwnedFd, deadline: Option<Instant>) -> bool {
    let n = match rustix::io::read(&s.from, &mut *buf) {
        Ok(0) => return false,
        Ok(n) => n,
        Err(Errno::INTR) => return true,
        Err(Errno::AGAIN) => return deadline.is_none(),
        Err(_) => return false,
    };
    if s.open {
        // SAFETY: stdout and stderr stay open for the life of the process.
        let to = unsafe { BorrowedFd::borrow_raw(s.to) };
        s.open = write_all(to, &buf[..n], stop, deadline);
    }
    true
}

/// Writes all of `buf`, waiting for the stream when it is full (a stream
/// someone set `O_NONBLOCK` on) until `deadline`, or, without one, until
/// `stop` is signalled and then for [`DRAIN_TIME`]. False when the stream
/// refuses or the time is up: its later output is read and dropped.
fn write_all(
    to: BorrowedFd<'_>,
    mut buf: &[u8],
    stop: &OwnedFd,
    deadline: Option<Instant>,
) -> bool {
    let mut deadline = deadline;
    while !buf.is_empty() {
        match rustix::io::write(to, buf) {
            Ok(n) => buf = &buf[n..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                let timeout = deadline.map(|d| {
                    let left = d.saturating_duration_since(Instant::now());
                    Timespec {
                        tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                        tv_nsec: i64::from(left.subsec_nanos()),
                    }
                });
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return false;
                }
                // The stop event is watched only until it fires: it stays
                // readable afterwards.
                let mut fds = [
                    PollFd::new(&to, PollFlags::OUT),
                    PollFd::new(stop, PollFlags::IN),
                ];
                let watched = if deadline.is_some() { 1 } else { 2 };
                match rustix::event::poll(&mut fds[..watched], timeout.as_ref()) {
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(_) => return false,
                }
                if deadline.is_none() && !fds[1].revents().is_empty() {
                    deadline = Some(Instant::now() + DRAIN_TIME);
                }
            }
            Err(_) => return false,
        }
    }
    true
}

fn set_nonblocking(fd: &OwnedFd) -> rustix::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)
}

#[cfg(test)]
impl Relay {
    fn has_thread(&self) -> bool {
        self.thread.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd as _;

    use super::*;

    /// A write end kept open after the `RUN` (here, by the test) never
    /// holds the relay: `finish` returns once what was written is copied.
    #[test]
    fn finish_does_not_wait_for_a_surviving_writer() {
        let mut stdio = [ChildStdio::Null, ChildStdio::Inherit, ChildStdio::Null];
        let mut keep = Vec::new();
        let relay = Relay::start(&mut stdio, &mut keep, Uid::ROOT, Gid::ROOT)
            .unwrap()
            .unwrap();
        assert!(relay.has_thread());
        assert!(matches!(stdio[1], ChildStdio::Fd(_)));
        assert_eq!(stdio[2], ChildStdio::Null);
        // The survivor: the write end stays open across `finish`.
        let survivor = keep.pop().unwrap();
        rustix::io::write(&survivor, b"relayed\n").unwrap();
        let start = Instant::now();
        relay.finish();
        assert!(start.elapsed() < DRAIN_TIME + Duration::from_secs(5));
        drop(survivor);
    }

    /// A writer that outlived the `RUN` and keeps writing (a fork loop the
    /// sweep could not end) never keeps the relay draining: `finish`
    /// returns after the drain time.
    #[test]
    fn finish_returns_while_a_survivor_keeps_writing() {
        let null = rustix::fs::open(
            "/dev/null",
            OFlags::WRONLY | OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let n = std::os::fd::AsRawFd::as_raw_fd(&null);
        let mut stdio = [ChildStdio::Null, ChildStdio::Inherit, ChildStdio::Null];
        let mut keep = Vec::new();
        let relay = Relay::start_into(&mut stdio, &mut keep, Uid::ROOT, Gid::ROOT, [n, n, n])
            .unwrap()
            .unwrap();
        let survivor = keep.pop().unwrap();
        // Writes until the pipe's read end is gone (EPIPE: Rust ignores
        // SIGPIPE).
        let writer = std::thread::spawn(move || {
            let chunk = [b'y'; 4096];
            while rustix::io::write(&survivor, &chunk).is_ok() {}
        });
        std::thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        relay.finish();
        assert!(
            start.elapsed() < DRAIN_TIME + Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        writer.join().unwrap();
        drop(null);
    }

    /// A stream that stays full gives up after the drain time.
    #[test]
    fn a_blocked_stream_is_given_up_after_the_drain_time() {
        let (out_r, out_w) =
            rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
        let stop = rustix::event::eventfd(0, EventfdFlags::CLOEXEC).unwrap();
        // Fill the output pipe.
        let chunk = [0u8; 4096];
        while rustix::io::write(&out_w, &chunk).is_ok() {}
        let start = Instant::now();
        rustix::io::write(&stop, &1u64.to_ne_bytes()).unwrap();
        assert!(!write_all(out_w.as_fd(), b"more", &stop, None));
        assert!(start.elapsed() < DRAIN_TIME + Duration::from_secs(5));
        drop(out_r);
    }

    #[test]
    fn nothing_inherited_needs_no_relay() {
        let mut stdio = [ChildStdio::Null; 3];
        let mut keep = Vec::new();
        assert!(
            Relay::start(&mut stdio, &mut keep, Uid::ROOT, Gid::ROOT)
                .unwrap()
                .is_none()
        );
        assert!(keep.is_empty());
    }
}
