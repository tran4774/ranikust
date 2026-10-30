//! `--jobs`: one pool of `RUN` permits shared by every stage.

use std::pin::pin;

use futures_util::future::{Either, select};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug_span};

/// The `--jobs` permits: a `RUN` holds one while its process tree runs.
#[derive(Debug)]
pub struct RunSlots {
    sem: Semaphore,
}

/// A held `--jobs` permit; dropping it frees the slot.
#[derive(Debug)]
pub struct RunPermit<'a> {
    _permit: SemaphorePermit<'a>,
}

impl RunSlots {
    /// `jobs` permits.
    #[must_use]
    pub(crate) fn new(jobs: std::num::NonZeroUsize) -> Self {
        Self {
            sem: Semaphore::new(jobs.get()),
        }
    }

    /// Permits free right now.
    #[must_use]
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// Waits for a permit, inside a `rnk_sched.wait_jobs` span; `None` once
    /// `cancel` fires.
    pub async fn acquire(&self, cancel: &CancellationToken) -> Option<RunPermit<'_>> {
        async {
            let cancelled = pin!(cancel.cancelled());
            let permit = pin!(self.sem.acquire());
            match select(cancelled, permit).await {
                Either::Left(((), _)) => None,
                // The semaphore is never closed, so `acquire` cannot fail;
                // a closed one would mean no permit, as a cancellation.
                Either::Right((p, _)) => p.ok().map(|p| RunPermit { _permit: p }),
            }
        }
        .instrument(debug_span!("rnk_sched.wait_jobs"))
        .await
    }
}
