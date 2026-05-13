// SPDX-License-Identifier: Apache-2.0
//! Shared utilities used across the objects crate's storage backends.
//!
//! `async_retry` requires tokio (only pulled in by feature-gated remote
//! backends like `s3`), so the module is gated to match.

#[cfg(feature = "s3")]
pub mod async_retry;

#[cfg(feature = "s3")]
pub use async_retry::{RetryDecision, RetryPolicy, classify_transient_io, retry_with};