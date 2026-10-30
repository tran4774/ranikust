//! Retries with exponential backoff.
//!
//! The defaults are go-containerregistry's, which kaniko gets for every
//! registry request whatever its `--push-retry`/`--image-download-retry`
//! (`kaniko@44aa7ddc0 vendor/github.com/google/go-containerregistry/pkg/v1/remote/options.go:80-101`):
//! three attempts, waiting 1 s then 3 s, plus up to 10 % jitter.

use std::hash::BuildHasher as _;
use std::time::Duration;

use crate::error::Error;

/// How failed requests are retried.
///
/// Only errors that may pass on their own are retried: connection failures
/// and timeouts, 408, 429, 500, 502, 503 and 504. Each attempt starts the
/// operation over (a new upload session, a pull from the first byte).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts in total, the first included; 1 never retries.
    pub attempts: u32,
    /// The wait before the second attempt.
    pub initial_delay: Duration,
    /// Each later wait is the one before times this.
    pub factor: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            initial_delay: Duration::from_secs(1),
            factor: 3,
        }
    }
}

impl RetryPolicy {
    /// No retries.
    pub fn none() -> Self {
        Self {
            attempts: 1,
            ..Self::default()
        }
    }

    /// kaniko's `--push-retry`/`--image-download-retry` `retries`: that many
    /// retries after the first attempt, waiting 1 s, then twice as long each
    /// time (`kaniko@44aa7ddc0 pkg/util/util.go:221` `Retry`). `0` never
    /// retries.
    pub fn kaniko(retries: u32) -> Self {
        Self {
            attempts: retries.saturating_add(1),
            initial_delay: Duration::from_secs(1),
            factor: 2,
        }
    }

    /// The wait after failed attempt `attempt` (0-based), without jitter.
    fn base_delay(&self, attempt: u32) -> Duration {
        let factor = self.factor.max(1).saturating_pow(attempt);
        self.initial_delay.saturating_mul(factor)
    }

    /// The wait after failed attempt `attempt`, with up to 10 % jitter so
    /// parallel transfers that failed together do not retry together.
    fn delay(&self, attempt: u32) -> Duration {
        let base = self.base_delay(attempt);
        // A per-process random seed is all the randomness jitter needs.
        let per_mille = std::collections::hash_map::RandomState::new().hash_one(attempt) % 101;
        let jitter = base.saturating_mul(u32::try_from(per_mille).unwrap_or(0)) / 1000;
        base.saturating_add(jitter)
    }
}

/// Runs `op` until it succeeds, fails with an error that is not retryable,
/// or runs out of attempts. `op` gets the 0-based attempt number.
///
/// Returns the result and the number of retries made.
pub(crate) async fn retry<T>(
    policy: &RetryPolicy,
    mut op: impl AsyncFnMut(u32) -> Result<T, Error>,
) -> (Result<T, Error>, u32) {
    let attempts = policy.attempts.max(1);
    let mut attempt = 0;
    loop {
        match op(attempt).await {
            Err(err) if err.is_retryable() && attempt + 1 < attempts => {
                tokio::time::sleep(policy.delay(attempt)).await;
                attempt += 1;
            }
            result => return (result, attempt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transient() -> Error {
        Error::Registry {
            operation: "pull blob",
            host: "r.example".to_owned(),
            target: "sha256:00".to_owned(),
            message: "HTTP 503".to_owned(),
            retryable: true,
        }
    }

    fn fast(attempts: u32) -> RetryPolicy {
        RetryPolicy {
            attempts,
            initial_delay: Duration::from_millis(1),
            factor: 2,
        }
    }

    #[test]
    fn default_delays_follow_go_containerregistry() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.base_delay(0), Duration::from_secs(1));
        assert_eq!(policy.base_delay(1), Duration::from_secs(3));
        for attempt in 0..2 {
            let delay = policy.delay(attempt);
            let base = policy.base_delay(attempt);
            assert!(delay >= base && delay <= base + base / 10, "{delay:?}");
        }
    }

    #[test]
    fn kaniko_retries_double_from_one_second() {
        let policy = RetryPolicy::kaniko(3);
        assert_eq!(policy.attempts, 4);
        assert_eq!(policy.base_delay(0), Duration::from_secs(1));
        assert_eq!(policy.base_delay(1), Duration::from_secs(2));
        assert_eq!(policy.base_delay(2), Duration::from_secs(4));
        assert_eq!(RetryPolicy::kaniko(0).attempts, 1);
    }

    #[tokio::test]
    async fn transient_errors_are_retried_until_success() {
        let mut calls = 0;
        let (result, retries) = retry(&fast(3), async |attempt| {
            calls += 1;
            if attempt < 2 {
                Err(transient())
            } else {
                Ok(attempt)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!((retries, calls), (2, 3));
    }

    #[tokio::test]
    async fn attempts_are_bounded() {
        let (result, retries) = retry(&fast(2), async |_| Err::<(), _>(transient())).await;
        assert!(result.unwrap_err().is_retryable());
        assert_eq!(retries, 1);
    }

    #[tokio::test]
    async fn permanent_errors_stop_at_once() {
        let mut calls = 0;
        let (result, retries) = retry(&fast(5), async |_| {
            calls += 1;
            Err::<(), _>(Error::Tls("x".to_owned()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!((retries, calls), (0, 1));
    }
}
