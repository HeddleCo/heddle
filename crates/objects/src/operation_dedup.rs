// SPDX-License-Identifier: Apache-2.0
//! Portable idempotency receipt vocabulary.

use crate::object::OperationId;
use serde::{Deserialize, Serialize};

/// Default retention for completed receipts. Pending reservations do not expire.
pub const DEFAULT_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// Hash the caller's canonical request bytes for deduplication.
pub fn hash_request_body(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// One persisted dedup entry. Identity is `(operation_id, verb)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupEntry {
    pub operation_id: OperationId,
    /// Hosted method name or CLI verb name, including the replay encoding
    /// generation when relevant. Operation IDs remain unique across the store;
    /// reusing one under a different verb is a conflict.
    pub verb: String,
    /// BLAKE3-256 of the request body bytes. The caller is responsible for
    /// choosing a deterministic encoding and including its generation in the
    /// verb whenever an encoding change would make cached data incompatible.
    pub request_hash: [u8; 32],
    /// Cached response bytes in the caller-owned encoding for this verb.
    /// Empty (`Vec::new()`) when [`pending`](Self::pending) is `true` —
    /// i.e. the slot is reserved but the response hasn't been recorded yet.
    pub response: Vec<u8>,
    /// Unix epoch seconds when this entry was created. Used by compaction.
    pub created_at_secs: i64,
    /// `true` when the entry is reserved but not yet completed.
    /// Concurrent retries with the same
    /// `(operation_id, verb)` see [`DedupOutcome::InFlight`] while the
    /// reservation is held. Cleared when the response is persisted or
    /// the reservation is canceled after execution fails.
    ///
    pub pending: bool,
}

/// Result of a dedup reservation call.
///
/// - [`DedupOutcome::Reserved`]: this id has not been seen, and the store
///   has atomically claimed the slot for the caller. The caller MUST
///   either complete the request or release the reservation. While
///   the reservation is held, concurrent identical requests see
///   [`DedupOutcome::InFlight`].
/// - [`DedupOutcome::Replay`]: a completed entry exists with a matching
///   body hash; the cached response is returned and the request must
///   *not* be re-executed.
/// - [`DedupOutcome::InFlight`]: a reservation for the same
///   `(operation_id, verb)` is currently held by another caller (with
///   the same body hash). The caller should surface a transient error
///   (`Status::aborted`) so the client can retry once the original
///   completes.
/// - [`DedupOutcome::Conflict`]: same id, different body. Caller should
///   surface a `FailedPrecondition` to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupOutcome {
    Reserved,
    Replay { response: Vec<u8> },
    InFlight,
    Conflict,
}

/// Safe-to-report metadata for an existing op-id slot. This deliberately
/// omits cached response bytes; callers use it to explain conflicts without
/// leaking command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupConflictMetadata {
    pub operation_id: OperationId,
    pub verb: String,
    pub request_hash: [u8; 32],
    pub created_at_secs: i64,
    pub pending: bool,
}
