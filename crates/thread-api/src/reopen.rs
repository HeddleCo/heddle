// SPDX-License-Identifier: Apache-2.0
//! Bounded reopen of a stale exact selection after weft v2 retryable signals.
use std::future::Future;

use api::{
    heddle::api::v1alpha1::{CallFailure, CallFailureCode},
    v2::client::ClientError,
};

use crate::transport::{Error, RemoteFailure};

/// Initial attempt plus four reopens of a fresh exact selection.
pub(crate) const ATTEMPTS: u32 = 5;

/// Weft fail-fasts a stale exact selection with these retryable signals.
/// A bare resend of the same selection would observe the same `Aborted`.
pub fn is_reopen_retryable(failure: &CallFailure) -> bool {
    retryable(failure.code(), &failure.message)
}

pub(crate) fn remote_failure_is_reopen_retryable(failure: &RemoteFailure) -> bool {
    retryable(
        CallFailureCode::try_from(failure.code).unwrap_or_default(),
        &failure.message,
    )
}

pub(crate) fn error_is_reopen_retryable(error: &Error) -> bool {
    match error {
        Error::Remote(failure) => remote_failure_is_reopen_retryable(failure),
        _ => false,
    }
}

pub(crate) fn client_error_is_reopen_retryable(error: &ClientError<Error>) -> bool {
    match error {
        ClientError::Transport(error) => error_is_reopen_retryable(error),
        _ => false,
    }
}

pub(crate) trait ReopenRetryable {
    fn is_reopen_retryable(&self) -> bool;
}

impl ReopenRetryable for Error {
    fn is_reopen_retryable(&self) -> bool {
        error_is_reopen_retryable(self)
    }
}

impl ReopenRetryable for ClientError<Error> {
    fn is_reopen_retryable(&self) -> bool {
        client_error_is_reopen_retryable(self)
    }
}

pub(crate) async fn retry<T, E, F, Fut>(mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: ReopenRetryable,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if error.is_reopen_retryable() && attempt + 1 < ATTEMPTS => {
                attempt += 1;
                backoff(attempt).await;
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) async fn backoff(attempt: u32) {
    let millis = 20u64.saturating_mul(u64::from(attempt));
    #[cfg(feature = "iroh")]
    {
        tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
    }
    #[cfg(not(feature = "iroh"))]
    {
        let _ = millis;
    }
}

fn retryable(code: CallFailureCode, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    let authority = message.contains("authority changed")
        || message.contains("reopen exact selection")
        || message.contains("authorization changed")
        || message.contains("is no longer valid");
    let pool = message.contains("pool timed out");
    match code {
        CallFailureCode::Aborted if authority || pool => true,
        CallFailureCode::Unavailable | CallFailureCode::ResourceExhausted if pool => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: CallFailureCode, message: &str) -> CallFailure {
        CallFailure {
            code: code as i32,
            message: message.into(),
            ..Default::default()
        }
    }

    #[test]
    fn classifies_weft_v2_authority_change_and_pool_timeout_signals() {
        assert!(is_reopen_retryable(&failure(
            CallFailureCode::Aborted,
            "material authority changed; reopen exact selection",
        )));
        assert!(is_reopen_retryable(&failure(
            CallFailureCode::Aborted,
            "authorization changed",
        )));
        assert!(is_reopen_retryable(&failure(
            CallFailureCode::Aborted,
            "authorization is no longer valid",
        )));
        assert!(is_reopen_retryable(&failure(
            CallFailureCode::Unavailable,
            "pool timed out while waiting for an open connection",
        )));
        assert!(is_reopen_retryable(&failure(
            CallFailureCode::ResourceExhausted,
            "pool timed out",
        )));
        assert!(is_reopen_retryable(&failure(
            CallFailureCode::Aborted,
            "pool timed out",
        )));
    }

    #[test]
    fn non_transient_failures_are_not_reopen_retryable() {
        assert!(!is_reopen_retryable(&failure(
            CallFailureCode::PermissionDenied,
            "material authority changed; reopen exact selection",
        )));
        assert!(!is_reopen_retryable(&failure(
            CallFailureCode::Aborted,
            "review required",
        )));
        assert!(!is_reopen_retryable(&failure(
            CallFailureCode::NotFound,
            "selected source unavailable",
        )));
        assert!(!is_reopen_retryable(&failure(
            CallFailureCode::Unavailable,
            "endpoint draining",
        )));
        assert!(!is_reopen_retryable(&failure(
            CallFailureCode::Internal,
            "pool timed out",
        )));
    }
}
