// SPDX-License-Identifier: Apache-2.0
//! Cross-cutting index over the operation log.
//!
//! Today the oplog is append-only and answers "what is the next entry?"
//! efficiently. It does not answer "operations by actor X touching symbol Y
//! with signal kind Z in time window T" without a linear scan. The
//! [`OperationLogIndex`] is the rebuildable sidecar that turns those queries
//! into bounded work.
//!
//! On-disk layout: `<heddle_dir>/cache/operation_index/` with one
//! day-bucketed file per UTC date and a top-level summary. Each file is
//! rmp-serde encoded. The directory is rebuildable from the oplog, so a
//! corrupted bucket is safe to delete.
//!
//! Querying is sequential scan over matching buckets within the requested
//! window, which is logarithmic-in-history when callers narrow by date and
//! linear-in-window otherwise — both bounded by the window, never by the
//! full repo lifetime.
//!
//! This module owns the index *shape* and the query primitives. Wiring (when
//! to write entries, when to rebuild) lands in `crates/cli/src/oplog_query.rs`
//! during the agent-first epic (A9/A10).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    object::{ChangeId, OperationId},
};
use serde::{Deserialize, Serialize};

const INDEX_FORMAT_VERSION: u8 = 1;
const INDEX_DIR_NAME: &str = "operation_index";

/// One indexed operation. Mirrors enough of `oplog::OpEntry` to satisfy the
/// query primitives without dragging in the full oplog crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedOperation {
    /// Sequential id from the oplog (monotonic per repo).
    pub seq: u64,
    /// Unix epoch seconds. Stored alongside the bucket date so the bucket
    /// file is self-describing without consulting its filename.
    pub timestamp_secs: i64,
    /// Operation verb name. Free-form so we can index across the W1
    /// `OpRecord` variant set without coupling to the oplog crate's enum.
    pub verb: String,
    /// Actor email, when known. Empty string for the system sentinel.
    pub actor_email: String,
    /// Client operation id, when one was supplied at call time.
    pub operation_id: Option<OperationId>,
    /// Thread the op happened on, when applicable (Snapshot, ThreadCreate,
    /// Checkpoint, EphemeralThreadCollapse, etc).
    pub thread: Option<String>,
    /// Symbols touched by this op, when the op carries them (e.g., the
    /// changed symbols in a Snapshot's diff). Free-form `<file>:<symbol>`
    /// strings to mirror [`objects::object::SignalAnchor::canonical`].
    pub symbols: Vec<String>,
    /// Risk-signal kinds fired on the resulting state, when applicable.
    /// Encoded as their snake-case wire string (e.g. `"novelty"`).
    pub signal_kinds: Vec<String>,
    /// Resulting state, when the op produced one.
    pub change_id: Option<ChangeId>,
}

impl IndexedOperation {
    pub fn timestamp(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.timestamp_secs, 0)
            .single()
            .unwrap_or_else(Utc::now)
    }

    pub fn date(&self) -> NaiveDate {
        self.timestamp().date_naive()
    }

    fn matches(&self, filter: &OperationLogQuery) -> bool {
        if let Some(actor) = &filter.actor
            && &self.actor_email != actor
        {
            return false;
        }
        if let Some(symbol) = &filter.symbol
            && !self.symbols.iter().any(|s| s == symbol)
        {
            return false;
        }
        if let Some(kind) = &filter.signal_kind
            && !self.signal_kinds.iter().any(|k| k == kind)
        {
            return false;
        }
        if let Some(thread) = &filter.thread
            && self.thread.as_deref() != Some(thread.as_str())
        {
            return false;
        }
        if let Some(ref verbs) = filter.verbs
            && !verbs.iter().any(|v| v == &self.verb)
        {
            return false;
        }
        if let Some(start) = filter.since
            && self.timestamp() < start
        {
            return false;
        }
        if let Some(end) = filter.until
            && self.timestamp() > end
        {
            return false;
        }
        true
    }
}

/// Filter shape for [`OperationLogIndex::query`].
#[derive(Debug, Clone, Default)]
pub struct OperationLogQuery {
    pub actor: Option<String>,
    pub symbol: Option<String>,
    pub signal_kind: Option<String>,
    pub thread: Option<String>,
    pub verbs: Option<Vec<String>>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DayBucket {
    format_version: u8,
    date: String,
    entries: Vec<IndexedOperation>,
}

impl DayBucket {
    fn new(date: NaiveDate) -> Self {
        Self {
            format_version: INDEX_FORMAT_VERSION,
            date: date.to_string(),
            entries: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.format_version > INDEX_FORMAT_VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "operation index bucket version {} > supported {}",
                self.format_version, INDEX_FORMAT_VERSION
            )));
        }
        Ok(())
    }
}

/// Top-level index. Multiple in-process instances of [`OperationLogIndex`]
/// pointing at the same `cache/operation_index/` directory are safe under
/// the documented append-only protocol — each `record` opens, mutates, and
/// rewrites a single bucket file atomically.
pub struct OperationLogIndex {
    root: PathBuf,
}

impl OperationLogIndex {
    /// Construct an index handle anchored at `<heddle_dir>/cache/operation_index/`.
    /// The directory is created on first write; opening a never-written
    /// index handle is free.
    pub fn new(heddle_dir: impl AsRef<Path>) -> Self {
        let root = heddle_dir.as_ref().join("cache").join(INDEX_DIR_NAME);
        Self { root }
    }

    /// Append an operation to the appropriate day-bucket. Idempotent on
    /// `(seq, timestamp_secs)` — if a bucket already contains an entry with
    /// the same sequence number it is replaced rather than duplicated.
    pub fn record(&self, op: IndexedOperation) -> Result<()> {
        let date = op.date();
        let mut bucket = self.load_bucket(date)?;
        if let Some(existing) = bucket.entries.iter_mut().find(|e| e.seq == op.seq) {
            *existing = op;
        } else {
            bucket.entries.push(op);
            bucket.entries.sort_by_key(|e| e.seq);
        }
        self.save_bucket(&bucket)
    }

    /// Run a structured query over the index. Buckets outside the
    /// `[since, until]` window are skipped without being read. Returns
    /// matches in seq order; `limit` truncates the head of the result.
    pub fn query(&self, filter: &OperationLogQuery) -> Result<Vec<IndexedOperation>> {
        let buckets = self.list_buckets()?;
        let mut results = Vec::new();
        for date in buckets {
            if !window_overlaps_date(filter.since, filter.until, date) {
                continue;
            }
            let bucket = self.load_bucket(date)?;
            for entry in bucket.entries {
                if entry.matches(filter) {
                    results.push(entry);
                }
            }
        }
        results.sort_by_key(|e| e.seq);
        if let Some(limit) = filter.limit {
            results.truncate(limit);
        }
        Ok(results)
    }

    /// Drop every persisted bucket. Used by [`Self::rebuild_from_iter`] and
    /// by the `0004_operation_index_initial_build` migration when wired up.
    pub fn clear(&self) -> Result<()> {
        if !self.root.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&self.root).map_err(HeddleError::from)?;
        Ok(())
    }

    /// Rebuild the index from an iterator of `IndexedOperation`. Drops every
    /// existing bucket first, then re-records each entry in order.
    pub fn rebuild_from_iter(&self, ops: impl IntoIterator<Item = IndexedOperation>) -> Result<()> {
        self.clear()?;
        // Group by date so each bucket file is written exactly once.
        let mut by_date: BTreeMap<NaiveDate, Vec<IndexedOperation>> = BTreeMap::new();
        for op in ops {
            by_date.entry(op.date()).or_default().push(op);
        }
        for (date, mut entries) in by_date {
            entries.sort_by_key(|e| e.seq);
            let bucket = DayBucket {
                format_version: INDEX_FORMAT_VERSION,
                date: date.to_string(),
                entries,
            };
            self.save_bucket(&bucket)?;
        }
        Ok(())
    }

    fn list_buckets(&self) -> Result<Vec<NaiveDate>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut dates = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(HeddleError::from)? {
            let entry = entry.map_err(HeddleError::from)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".bin") else {
                continue;
            };
            if let Ok(date) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
                dates.push(date);
            }
        }
        dates.sort();
        Ok(dates)
    }

    fn bucket_path(&self, date: NaiveDate) -> PathBuf {
        self.root.join(format!("{date}.bin"))
    }

    fn load_bucket(&self, date: NaiveDate) -> Result<DayBucket> {
        let path = self.bucket_path(date);
        if !path.exists() {
            return Ok(DayBucket::new(date));
        }
        let bytes = fs::read(&path).map_err(HeddleError::from)?;
        let bucket: DayBucket = rmp_serde::from_slice(&bytes).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "operation index bucket at {} is malformed: {err}",
                path.display()
            ))
        })?;
        bucket.validate()?;
        Ok(bucket)
    }

    fn save_bucket(&self, bucket: &DayBucket) -> Result<()> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root).map_err(HeddleError::from)?;
        }
        let date = NaiveDate::parse_from_str(&bucket.date, "%Y-%m-%d").map_err(|err| {
            HeddleError::InvalidObject(format!("invalid bucket date '{}': {err}", bucket.date))
        })?;
        let path = self.bucket_path(date);
        let bytes = rmp_serde::to_vec(bucket).map_err(|err| {
            HeddleError::InvalidObject(format!("failed to encode operation index bucket: {err}"))
        })?;
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }
}

fn window_overlaps_date(
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    date: NaiveDate,
) -> bool {
    let day_start = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("valid hour"));
    let day_end = day_start + chrono::Duration::days(1);
    if let Some(start) = since
        && day_end <= start
    {
        return false;
    }
    if let Some(end) = until
        && day_start > end
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn make_op(seq: u64, ts_secs: i64, actor: &str, symbol: &str) -> IndexedOperation {
        IndexedOperation {
            seq,
            timestamp_secs: ts_secs,
            verb: "snapshot".into(),
            actor_email: actor.to_string(),
            operation_id: None,
            thread: Some("main".into()),
            symbols: vec![symbol.to_string()],
            signal_kinds: vec![],
            change_id: Some(ChangeId::from_bytes([1; 16])),
        }
    }

    fn fresh_index() -> (TempDir, OperationLogIndex) {
        let temp = TempDir::new().unwrap();
        let index = OperationLogIndex::new(temp.path());
        (temp, index)
    }

    #[test]
    fn record_and_query_actor_within_window() {
        let (_t, index) = fresh_index();
        index
            .record(make_op(
                1,
                1_700_000_000,
                "alice@example.com",
                "src/lib.rs:foo",
            ))
            .unwrap();
        index
            .record(make_op(
                2,
                1_700_086_400,
                "bob@example.com",
                "src/lib.rs:bar",
            ))
            .unwrap();
        let q = OperationLogQuery {
            actor: Some("alice@example.com".into()),
            ..Default::default()
        };
        let hits = index.query(&q).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 1);
    }

    #[test]
    fn query_with_no_match_returns_empty() {
        let (_t, index) = fresh_index();
        index
            .record(make_op(1, 1_700_000_000, "alice@example.com", "a"))
            .unwrap();
        let q = OperationLogQuery {
            actor: Some("nobody@example.com".into()),
            ..Default::default()
        };
        assert!(index.query(&q).unwrap().is_empty());
    }

    #[test]
    fn record_is_idempotent_on_seq() {
        let (_t, index) = fresh_index();
        index
            .record(make_op(1, 1_700_000_000, "alice", "a"))
            .unwrap();
        index
            .record(make_op(1, 1_700_000_000, "alice", "a"))
            .unwrap();
        let hits = index.query(&OperationLogQuery::default()).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rebuild_drops_and_reseeds() {
        let (_t, index) = fresh_index();
        index
            .record(make_op(1, 1_700_000_000, "alice", "a"))
            .unwrap();
        index.record(make_op(2, 1_700_086_400, "bob", "b")).unwrap();
        index
            .rebuild_from_iter(vec![make_op(7, 1_700_000_000, "carol", "c")])
            .unwrap();
        let hits = index.query(&OperationLogQuery::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 7);
    }

    #[test]
    fn time_window_filters_buckets() {
        let (_t, index) = fresh_index();
        // Day 1
        index
            .record(make_op(1, 1_700_000_000, "alice", "a"))
            .unwrap();
        // Day 5
        index
            .record(make_op(2, 1_700_000_000 + 5 * 86_400, "alice", "b"))
            .unwrap();
        let since = Utc
            .timestamp_opt(1_700_000_000 + 3 * 86_400, 0)
            .single()
            .unwrap();
        let q = OperationLogQuery {
            since: Some(since),
            ..Default::default()
        };
        let hits = index.query(&q).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 2);
    }

    #[test]
    fn limit_truncates_results() {
        let (_t, index) = fresh_index();
        for i in 0..5 {
            index
                .record(make_op(i, 1_700_000_000 + (i as i64) * 60, "a", "s"))
                .unwrap();
        }
        let q = OperationLogQuery {
            limit: Some(2),
            ..Default::default()
        };
        let hits = index.query(&q).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].seq, 0);
        assert_eq!(hits[1].seq, 1);
    }

    #[test]
    fn corrupt_bucket_is_a_decode_error_not_a_panic() {
        let (_t, index) = fresh_index();
        index
            .record(make_op(1, 1_700_000_000, "alice", "a"))
            .unwrap();
        let buckets = index.list_buckets().unwrap();
        let path = index.bucket_path(buckets[0]);
        std::fs::write(&path, b"not valid rmp").unwrap();
        let result = index.query(&OperationLogQuery::default());
        assert!(result.is_err(), "expected error for corrupt bucket");
    }
}