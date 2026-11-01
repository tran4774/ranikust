//! The fence between `RUN`s and named state (kaniko mode on `/`, the
//! decision answering Q-340).
//!
//! Where the state directory's filesystem has no `O_TMPFILE` (the overlayfs
//! root of a container), an anonymous file is made on disk by a random name
//! and unlinked at once; a stage's kept tree is unpacked for a `COPY --from`
//! into a named directory and deleted before the next `RUN`. A `RUN` on `/`
//! runs as root on that filesystem, so a name must never exist while a
//! `RUN` process does. The fence holds the two apart:
//!
//! - [`Store::enter_run`](crate::Store::enter_run) waits for every name to be gone, then counts a
//!   `RUN` in until its guard drops, which the caller does once the `RUN`'s
//!   process tree is gone (the sweep ends inside `Sandbox::run`).
//! - [`Store::named_window`](crate::Store::named_window) waits for every `RUN` to be gone, then counts a name
//!   in until its guard drops, which the holder does once the name is
//!   unlinked or the directory deleted.
//!
//! A name asked for on a thread that is inside a `RUN`, or a `RUN` started
//! while a name has outlived [`WAIT_FOR_NAMES`], is a bug in ranikust's
//! order of work, not a race to wait out: both fail closed with an error.
//! A `RUN` that ended with an error ([`Fence::poison`]: the sweep of its
//! process tree may not have finished) closes the fence to names for the
//! rest of the build: its processes may live on.
//! In every other store the fence is open and its guards do nothing.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// How long a `RUN` waits for names to go: a file's name lives for one
/// `openat` and `unlinkat`, an unpacked directory for the instruction that
/// reads it; longer means one outlived its instruction, a bug.
pub(crate) const WAIT_FOR_NAMES: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct State {
    /// Threads inside a `RUN`, one entry per guard.
    runs: Vec<ThreadId>,
    /// Names alive.
    names: usize,
    /// A `RUN` ended with an error: no name from now on.
    poisoned: bool,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

/// See the module doc. Cheap to clone; clones share one fence.
#[derive(Clone, Debug, Default)]
pub(crate) struct Fence {
    /// `None`: an open fence, for stores where nothing a `RUN` can reach is
    /// ever named (a persistent store).
    inner: Option<Arc<Inner>>,
}

impl Fence {
    /// A closed fence: names and `RUN`s exclude each other.
    pub(crate) fn closed() -> Self {
        Self {
            inner: Some(Arc::default()),
        }
    }

    /// Counts a `RUN` in, once no name is alive.
    pub(crate) fn enter_run(&self) -> Result<RunGuard> {
        let Some(inner) = &self.inner else {
            return Ok(RunGuard { inner: None });
        };
        let deadline = Instant::now() + WAIT_FOR_NAMES;
        let mut state = lock(inner);
        while state.names > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Anonymous("a RUN while named state is still on disk"));
            }
            state = inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.runs.push(std::thread::current().id());
        Ok(RunGuard {
            inner: Some(Arc::clone(inner)),
        })
    }

    /// Counts a name in, once no `RUN` is alive. Waits as long as a `RUN`
    /// runs: a pull for a later stage waits for the current `RUN` to end.
    pub(crate) fn named(&self) -> Result<NamedGuard> {
        let Some(inner) = &self.inner else {
            return Ok(NamedGuard { inner: None });
        };
        let me = std::thread::current().id();
        let mut state = lock(inner);
        if state.poisoned {
            return Err(Error::Anonymous(
                "named state after a RUN whose processes may still live",
            ));
        }
        while !state.runs.is_empty() {
            if state.runs.contains(&me) {
                return Err(Error::Anonymous("named state made during a RUN"));
            }
            state = inner
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.poisoned {
            return Err(Error::Anonymous(
                "named state after a RUN whose processes may still live",
            ));
        }
        state.names += 1;
        Ok(NamedGuard {
            inner: Some(Arc::clone(inner)),
        })
    }
}

impl Fence {
    /// Closes the fence to names for good: a `RUN` ended with an error,
    /// and its processes may live.
    pub(crate) fn poison(&self) {
        if let Some(inner) = &self.inner {
            lock(inner).poisoned = true;
            inner.changed.notify_all();
        }
    }

    /// Whether [`Fence::poison`] was called.
    pub(crate) fn is_poisoned(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| lock(i).poisoned)
    }
}

fn lock(inner: &Inner) -> MutexGuard<'_, State> {
    inner.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A `RUN` counted in by [`Store::enter_run`](crate::Store::enter_run); dropping it counts it out.
#[derive(Debug)]
#[must_use = "the RUN is counted out when the guard drops"]
pub struct RunGuard {
    inner: Option<Arc<Inner>>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Some(inner) = &self.inner {
            let me = std::thread::current().id();
            let mut state = lock(inner);
            if let Some(i) = state.runs.iter().position(|t| *t == me) {
                state.runs.swap_remove(i);
            } else {
                // Dropped on another thread: count one `RUN` out anyway.
                state.runs.pop();
            }
            drop(state);
            inner.changed.notify_all();
        }
    }
}

/// A name counted in by [`Store::named_window`](crate::Store::named_window); drop it once the name is gone.
#[derive(Debug)]
#[must_use = "the name is counted out when the guard drops"]
pub struct NamedGuard {
    inner: Option<Arc<Inner>>,
}

impl Drop for NamedGuard {
    fn drop(&mut self) {
        if let Some(inner) = &self.inner {
            let mut state = lock(inner);
            state.names = state.names.saturating_sub(1);
            drop(state);
            inner.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn an_open_fence_never_waits() {
        let f = Fence::default();
        let _run = f.enter_run().unwrap();
        let _name = f.named().unwrap();
    }

    #[test]
    fn a_name_waits_for_the_run_to_end() {
        let f = Fence::closed();
        let run = f.enter_run().unwrap();
        let (tx, rx) = mpsc::channel();
        let g = f.clone();
        let t = std::thread::spawn(move || {
            let name = g.named().unwrap();
            tx.send(()).unwrap();
            drop(name);
        });
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(run);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn a_run_waits_for_the_name_to_go() {
        let f = Fence::closed();
        let name = f.named().unwrap();
        let (tx, rx) = mpsc::channel();
        let g = f.clone();
        let t = std::thread::spawn(move || {
            let run = g.enter_run().unwrap();
            tx.send(()).unwrap();
            drop(run);
        });
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(name);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn a_poisoned_fence_refuses_every_name() {
        let f = Fence::closed();
        f.poison();
        assert!(f.is_poisoned());
        assert!(matches!(f.named(), Err(Error::Anonymous(_))));
        // RUNs still run: their state is anonymous.
        let _run = f.enter_run().unwrap();
    }

    #[test]
    fn a_name_inside_a_run_on_the_same_thread_fails_closed() {
        let f = Fence::closed();
        let _run = f.enter_run().unwrap();
        assert!(matches!(f.named(), Err(Error::Anonymous(_))));
    }
}
