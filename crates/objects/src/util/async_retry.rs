// SPDX-License-Identifier: Apache-2.0
//! Backoff-retry helper for transient remote-storage failures.
//!
//! Originally inlined in `store::s3::s3_impl::helpers`. Lifted here so any
//! future remote backend (GCS, Azure, custom) can share the loop and the
//! transient-error classification heuristics without each backend rolling
//! its own.

use std::{future::Future, time::Duration};

use tokio::time::sleep;

/// What to do when an operation fails on attempt N.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    Retry,
    DoNotRetry,
}

/// Exponential-backoff policy. Captures the bound on attempts plus the
/// shape of the backoff curve.
///
/// `max_attempts` is inclusive of the initial try, so `max_attempts = 4`
/// means "one try + three retries".
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub initial_backoff: Duration,
    pub backoff_multiplier: u32,
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// Default for remote object stores: 4 attempts total, 50ms → 100ms →
    /// 200ms, capped at 5s. Matches the historic S3 backoff exactly.
    pub const S3_DEFAULT: Self = Self {
        max_attempts: 4,
        initial_backoff: Duration::from_millis(50),
        backoff_multiplier: 2,
        max_backoff: Duration::from_secs(5),
    };
}

/// Run `op` under `policy` until it succeeds, returns a non-retryable
/// error per `classify`, or runs out of attempts.
///
/// The classifier is given the error by reference so callers can match
/// against backend-specific error variants without owning the value.
pub async fn retry_with<F, Fut, T, E>(
    policy: RetryPolicy,
    classify: impl Fn(&E) -> RetryDecision,
    mut op: F,
) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let mut delay = policy.initial_backoff;
    let max_attempts = policy.max_attempts.max(1);

    for attempt in 0..max_attempts {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) if attempt + 1 < max_attempts && classify(&err) == RetryDecision::Retry => {
                sleep(delay).await;
                delay = delay
                    .saturating_mul(policy.backoff_multiplier)
                    .min(policy.max_backoff);
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!("retry loop always returns success or error")
}

/// Substring-match heuristic over `io::Error::to_string()` to classify
/// transient network failures from object stores. Shared so any backend
/// that funnels its errors through `io::Error` gets the same behavior.
pub fn classify_transient_io(error: &std::io::Error) -> RetryDecision {
    let message = error.to_string().to_ascii_lowercase();
    let is_transient = [
        "500",
        "502",
        "503",
        "504",
        "internal server error",
        "service unavailable",
        "slowdown",
        "throttl",
        "connection reset",
        "connection aborted",
        "broken pipe",
        "timeout",
        "timed out",
    ]
    .iter()
    .any(|pattern| message.contains(pattern));

    if is_transient {
        RetryDecision::Retry
    } else {
        RetryDecision::DoNotRetry
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn retries_until_success() {
        let calls = AtomicUsize::new(0);
        let result: Result<&str, &str> = retry_with(
            RetryPolicy {
                max_attempts: 4,
                initial_backoff: Duration::from_millis(1),
                backoff_multiplier: 2,
                max_backoff: Duration::from_millis(10),
            },
            |_| RetryDecision::Retry,
            || async {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 { Err("flaky") } else { Ok("ok") }
            },
        )
        .await;
        assert_eq!(result, Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = AtomicUsize::new(0);
        let result: Result<&str, &str> = retry_with(
            RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(1),
                backoff_multiplier: 2,
                max_backoff: Duration::from_millis(10),
            },
            |_| RetryDecision::Retry,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<&str, _>("always fails")
            },
        )
        .await;
        assert_eq!(result, Err("always fails"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_retryable_returns_immediately() {
        let calls = AtomicUsize::new(0);
        let result: Result<&str, &str> = retry_with(
            RetryPolicy {
                max_attempts: 4,
                initial_backoff: Duration::from_millis(1),
                backoff_multiplier: 2,
                max_backoff: Duration::from_millis(10),
            },
            |_| RetryDecision::DoNotRetry,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<&str, _>("permanent")
            },
        )
        .await;
        assert_eq!(result, Err("permanent"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn classifies_http_5xx_as_transient() {
        let err = std::io::Error::other("503 Service Unavailable");
        assert_eq!(classify_transient_io(&err), RetryDecision::Retry);
    }

    #[test]
    fn classifies_local_io_as_permanent() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(classify_transient_io(&err), RetryDecision::DoNotRetry);
    }
}