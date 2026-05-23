// SPDX-License-Identifier: Apache-2.0
//! Idempotency dedup store for `client_operation_id`.
//!
//! Every state-changing CLI verb and gRPC method accepts an optional
//! `client_operation_id` (UUID v4). The first time the server sees an id it
//! processes the request and persists `(operation_id, request_hash, response)`.
//! If the same id arrives again with the same body hash, the server returns
//! the cached response bit-identical without re-executing. If the body
//! differs, the server returns `FailedPrecondition` so the caller can detect
//! the bug.
//!
//! This module owns the local file-backed store. Persisted layout:
//! `<heddle_dir>/state/operation_dedup.bin` — rmp-serde encoded
//! [`DedupStore`]. A periodic compaction pass (run from the maintenance
//! routine) prunes entries older than the configured retention window.
//!
//! The hosted server uses a Postgres table with the same logical schema; see
//! `crates/server/src/server/grpc_hosted_impl/idempotency.rs` for that
//! adapter (W2). Both share the [`DedupOutcome`] return type so the
//! middleware code is identical regardless of backend.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    object::OperationId,
};
use serde::{Deserialize, Serialize};

const DEDUP_FORMAT_VERSION: u8 = 1;
const DEDUP_FILE_NAME: &str = "operation_dedup.bin";
/// Default retention. Configurable via `[idempotency] retention_days` in
/// repo config; that wiring lives in the server crate.
pub const DEFAULT_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// One persisted dedup entry. Identity is `(operation_id, verb)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupEntry {
    pub operation_id: OperationId,
    /// gRPC method name or CLI verb name. Lets two distinct verbs share an
    /// operation id without colliding (rare but supported).
    pub verb: String,
    /// BLAKE3-256 of the request body bytes. The server is responsible for
    /// producing a canonical encoding before hashing — usually the prost-
    /// encoded protobuf bytes.
    pub request_hash: [u8; 32],
    /// Cached response bytes. Same canonical encoding as the request.
    /// Empty (`Vec::new()`) when [`pending`](Self::pending) is `true` —
    /// i.e. the slot is reserved but the response hasn't been recorded yet.
    pub response: Vec<u8>,
    /// Unix epoch seconds when this entry was created. Used by compaction.
    pub created_at_secs: i64,
    /// `true` when the entry is a reservation written by
    /// [`OperationDedupStore::reserve`] but not yet finalised by
    /// [`OperationDedupStore::record`]. Concurrent retries with the same
    /// `(operation_id, verb)` see [`DedupOutcome::InFlight`] while the
    /// reservation is held. Cleared by `record` (when the response is
    /// persisted) or [`OperationDedupStore::cancel`] (on execute failure).
    ///
    /// `#[serde(default)]` so existing on-disk dedup files (which never had
    /// this field) decode as `pending = false` — the entries they describe
    /// are completed records.
    #[serde(default)]
    pub pending: bool,
}

/// On-disk root of the dedup store. Wrapped by [`OperationDedupStore`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DedupFile {
    format_version: u8,
    /// Keyed by `format!("{verb}/{operation_id}")` so verb collisions are
    /// resolved at the key level. `BTreeMap` gives deterministic on-disk
    /// ordering for stable round-trip.
    entries: BTreeMap<String, DedupEntry>,
}

/// Result of a [`OperationDedupStore::reserve`] call.
///
/// - [`DedupOutcome::Reserved`]: this id has not been seen, and the store
///   has atomically claimed the slot for the caller. The caller MUST
///   either complete the request via [`OperationDedupStore::record`] or
///   release the reservation via [`OperationDedupStore::cancel`]. While
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

/// File-backed dedup store. Cheap to clone — the heavy lifting is behind a
/// [`Mutex`] that serializes writes.
pub struct OperationDedupStore {
    path: PathBuf,
    inner: Mutex<DedupFile>,
}

impl OperationDedupStore {
    /// Open (or initialise) the store at `<heddle_dir>/state/operation_dedup.bin`.
    pub fn open(heddle_dir: impl AsRef<Path>) -> Result<Self> {
        let path = heddle_dir.as_ref().join("state").join(DEDUP_FILE_NAME);
        let inner = if path.exists() {
            let bytes = std::fs::read(&path).map_err(HeddleError::from)?;
            let file: DedupFile = rmp_serde::from_slice(&bytes).map_err(|err| {
                HeddleError::InvalidObject(format!(
                    "operation_dedup.bin at {} is malformed: {err}",
                    path.display()
                ))
            })?;
            if file.format_version > DEDUP_FORMAT_VERSION {
                return Err(HeddleError::InvalidObject(format!(
                    "operation dedup format version {} > supported {}",
                    file.format_version, DEDUP_FORMAT_VERSION
                )));
            }
            file
        } else {
            DedupFile {
                format_version: DEDUP_FORMAT_VERSION,
                entries: BTreeMap::new(),
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// Probe the store and atomically claim a slot if no entry exists.
    ///
    /// This collapses the old `check` + `execute` + `record` flow's race
    /// window: previously two concurrent retries with the same
    /// `client_operation_id` could both observe `Fresh`, both execute, and
    /// both apply side effects before either persisted a record. `reserve`
    /// inserts a [`DedupEntry`] with `pending = true` under the same
    /// `Mutex` that gates `record`, so subsequent callers see
    /// [`DedupOutcome::InFlight`] (matching body) or
    /// [`DedupOutcome::Conflict`] (mismatched body).
    ///
    /// Caller contract: when [`DedupOutcome::Reserved`] is returned, the
    /// caller MUST follow up with either [`Self::record`] (on success) or
    /// [`Self::cancel`] (on failure) — otherwise the slot remains held
    /// until the next compaction sweep.
    pub fn reserve(
        &self,
        operation_id: OperationId,
        verb: &str,
        request_hash: [u8; 32],
    ) -> Result<DedupOutcome> {
        let key = key_for(verb, operation_id);
        let mut inner = self.inner.lock().expect("dedup mutex poisoned");
        match inner.entries.get(&key) {
            Some(existing) if existing.pending && existing.request_hash == request_hash => {
                Ok(DedupOutcome::InFlight)
            }
            Some(existing) if existing.request_hash == request_hash => Ok(DedupOutcome::Replay {
                response: existing.response.clone(),
            }),
            Some(_) => Ok(DedupOutcome::Conflict),
            None => {
                let entry = DedupEntry {
                    operation_id,
                    verb: verb.to_string(),
                    request_hash,
                    response: Vec::new(),
                    created_at_secs: now_secs(),
                    pending: true,
                };
                inner.entries.insert(key, entry);
                self.persist(&inner)?;
                Ok(DedupOutcome::Reserved)
            }
        }
    }

    /// Persist the response for an executed request, finalising a
    /// [`DedupOutcome::Reserved`] slot. Idempotent: rewriting an existing
    /// entry with identical body is a no-op (`created_at_secs` updates if
    /// the new write is later).
    pub fn record(
        &self,
        operation_id: OperationId,
        verb: &str,
        request_hash: [u8; 32],
        response: Vec<u8>,
    ) -> Result<()> {
        let key = key_for(verb, operation_id);
        let entry = DedupEntry {
            operation_id,
            verb: verb.to_string(),
            request_hash,
            response,
            created_at_secs: now_secs(),
            pending: false,
        };
        let mut inner = self.inner.lock().expect("dedup mutex poisoned");
        inner.entries.insert(key, entry);
        self.persist(&inner)
    }

    /// Release a reservation without persisting a response. Called when
    /// the caller's `execute` step fails — the slot needs to be freed so
    /// retries can claim it. No-op if no reservation exists or the entry
    /// has already been finalised by [`Self::record`].
    pub fn cancel(&self, operation_id: OperationId, verb: &str) -> Result<()> {
        let key = key_for(verb, operation_id);
        let mut inner = self.inner.lock().expect("dedup mutex poisoned");
        if let Some(existing) = inner.entries.get(&key)
            && existing.pending
        {
            inner.entries.remove(&key);
            self.persist(&inner)?;
        }
        Ok(())
    }

    /// Drop entries older than `retention_secs`. Returns the number of
    /// pruned entries.
    pub fn compact(&self, retention_secs: i64) -> Result<usize> {
        let cutoff = now_secs() - retention_secs;
        let mut inner = self.inner.lock().expect("dedup mutex poisoned");
        let before = inner.entries.len();
        inner.entries.retain(|_, e| e.created_at_secs >= cutoff);
        let pruned = before - inner.entries.len();
        if pruned > 0 {
            self.persist(&inner)?;
        }
        Ok(pruned)
    }

    /// Total entries currently stored. Mostly useful for tests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("dedup mutex poisoned")
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn persist(&self, inner: &DedupFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(HeddleError::from)?;
        }
        let bytes = rmp_serde::to_vec(inner).map_err(|err| {
            HeddleError::InvalidObject(format!("failed to encode operation dedup file: {err}"))
        })?;
        write_file_atomic(&self.path, &bytes)?;
        Ok(())
    }
}

fn key_for(verb: &str, operation_id: OperationId) -> String {
    format!("{verb}/{operation_id}")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compute the canonical request hash. Helper centralising the hashing
/// scheme so all callers (CLI verbs, gRPC handlers) hash identically.
pub fn hash_request_body(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn make_store() -> (TempDir, OperationDedupStore) {
        let temp = TempDir::new().unwrap();
        // Mimic the layout `Repository::open` would produce.
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let store = OperationDedupStore::open(&heddle).unwrap();
        (temp, store)
    }

    #[test]
    fn reserve_then_record_then_replay() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let body = b"hello";
        let hash = hash_request_body(body);

        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        store
            .record(op, "capture", hash, b"response-1".to_vec())
            .unwrap();

        match store.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"response-1"),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn second_reserve_with_same_body_sees_in_flight() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::InFlight
        );
    }

    #[test]
    fn cancel_releases_reservation() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        store.cancel(op, "capture").unwrap();
        // Slot is now free; a follow-up retry can re-claim it.
        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
    }

    #[test]
    fn cancel_does_not_clobber_completed_record() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        // cancel must be a no-op against finalised entries — otherwise a
        // late-arriving cancel from a crashed retry could wipe the cached
        // response of a successful prior call.
        store.cancel(op, "capture").unwrap();
        match store.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"r"),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn conflict_on_different_body() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash_a = hash_request_body(b"a");
        let hash_b = hash_request_body(b"b");

        store
            .record(op, "capture", hash_a, b"resp".to_vec())
            .unwrap();
        assert_eq!(
            store.reserve(op, "capture", hash_b).unwrap(),
            DedupOutcome::Conflict
        );
    }

    #[test]
    fn different_verbs_do_not_collide() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r1".to_vec()).unwrap();
        assert_eq!(
            store.reserve(op, "merge", hash).unwrap(),
            DedupOutcome::Reserved
        );
    }

    #[test]
    fn persists_across_reopen() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        {
            let store = OperationDedupStore::open(&heddle).unwrap();
            store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        }
        let store = OperationDedupStore::open(&heddle).unwrap();
        match store.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"r"),
            other => panic!("expected replay after reopen, got {other:?}"),
        }
    }

    #[test]
    fn compact_drops_old_entries() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        assert_eq!(store.len(), 1);
        // Retain only entries newer than 0 seconds — everything older than
        // "now" is technically fair game. We pick a tiny retention to force
        // compaction while still inside the test.
        let pruned = store.compact(-1).unwrap();
        assert_eq!(pruned, 1);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn fresh_after_compaction() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        store.compact(-1).unwrap();
        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
    }
}
