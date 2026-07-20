// SPDX-License-Identifier: Apache-2.0
//! Heddle local typed review operations.
//!
//! The CLI calls this implementation in-process against one repository; it
//! has no socket, transport, or daemon lifecycle.

pub mod local_review;
