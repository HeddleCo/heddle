// SPDX-License-Identifier: Apache-2.0
//! Tests for the S3 retry path.
//!
//! The retry primitives moved out of `super::helpers` into
//! [`crate::util::async_retry`] when the workspace gained the generic
//! `retry_with` / `classify_transient_io` API. Tests use the same
//! `RetryPolicy::S3_DEFAULT` the production paths use so backoff
//! shape and attempt counts match the live behaviour.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::util::{RetryDecision, RetryPolicy, classify_transient_io, retry_with};

#[tokio::test]
async fn test_retry_s3_operation_retries_transient_failures() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_op = Arc::clone(&attempts);

    let result = retry_with(RetryPolicy::S3_DEFAULT, classify_transient_io, || {
        let attempts = Arc::clone(&attempts_for_op);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                Err(io::Error::other("503 service unavailable"))
            } else {
                Ok::<_, io::Error>("ok")
            }
        }
    })
    .await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_retry_s3_operation_does_not_retry_permanent_failures() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_op = Arc::clone(&attempts);

    let result = retry_with(RetryPolicy::S3_DEFAULT, classify_transient_io, || {
        let attempts = Arc::clone(&attempts_for_op);
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(io::Error::other("403 forbidden"))
        }
    })
    .await;

    assert!(result.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn test_should_retry_io_error_classifies_transient_failures() {
    assert_eq!(
        classify_transient_io(&io::Error::other("500 internal server error")),
        RetryDecision::Retry
    );
    assert_eq!(
        classify_transient_io(&io::Error::other("SlowDown throttling")),
        RetryDecision::Retry
    );
    assert_eq!(
        classify_transient_io(&io::Error::other("connection reset by peer")),
        RetryDecision::Retry
    );
    assert_eq!(
        classify_transient_io(&io::Error::other("404 not found")),
        RetryDecision::DoNotRetry
    );
}