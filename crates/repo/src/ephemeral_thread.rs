// SPDX-License-Identifier: Apache-2.0
//! Auto-collapse for ephemeral threads.
//!
//! Threads spawned with `heddle thread spawn --ephemeral --ttl <duration>`
//! carry an [`EphemeralMarker`](crate::EphemeralMarker) at the tail of their
//! [`ThreadRecord`](crate::ThreadRecord). When the TTL elapses without the
//! thread being promoted, the next time the CLI walks open threads (during
//! `status`, `log`, `thread list`) it sweeps expired markers, sets each
//! thread to [`ThreadState::Abandoned`], and records an
//! `OpRecord::EphemeralThreadCollapse` so the underlying states stay
//! addressable while the thread pointer retires.
//!
//! This module is pure with respect to time: callers pass `now` so tests can
//! drive expiry without mocking the clock.

use chrono::{DateTime, Utc};
use objects::object::StateId;

use crate::{ThreadRecord, ThreadState};

/// One thread's collapse outcome. Returned by [`collapse_expired_ephemeral_threads`]
/// so callers (CLI, daemon) can record the matching `OpRecord` entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsedThread {
    pub thread_id: String,
    pub thread_name: String,
    pub final_state: Option<StateId>,
    pub expired_at: DateTime<Utc>,
}

/// Walk a list of thread records and identify those whose ephemeral TTL has
/// elapsed and whose `auto_collapse` is still on. Returns the records mutated
/// in place (state set to `Abandoned`) plus a [`CollapsedThread`] entry per
/// transition for the caller to log.
///
/// Pure (no I/O). The caller is responsible for persisting the mutated
/// records and recording the matching oplog entries — see
/// `crates/cli/src/cli/commands/ephemeral_sweep.rs` (A13) for the wiring.
pub fn collapse_expired_ephemeral_threads(
    records: &mut [ThreadRecord],
    now: DateTime<Utc>,
) -> Vec<CollapsedThread> {
    let mut collapsed = Vec::new();
    for record in records.iter_mut() {
        let Some(marker) = record.ephemeral.as_ref() else {
            continue;
        };
        if !marker.auto_collapse {
            continue;
        }
        if !marker.is_expired_at(now) {
            continue;
        }
        // Already in a terminal state — don't double-collapse.
        if matches!(
            record.state,
            ThreadState::Abandoned | ThreadState::Merged | ThreadState::Promoted
        ) {
            continue;
        }
        let final_state = record
            .current_state
            .as_deref()
            .and_then(|s| StateId::parse(s).ok())
            .or_else(|| StateId::parse(&record.base_state).ok());
        collapsed.push(CollapsedThread {
            thread_id: record.id.clone(),
            thread_name: record.thread.clone(),
            final_state,
            expired_at: marker.expires_at(),
        });
        record.state = ThreadState::Abandoned;
        record.updated_at = now;
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::EphemeralMarker;

    fn make_record(id: &str, marker: Option<EphemeralMarker>, state: ThreadState) -> ThreadRecord {
        let now = Utc::now();
        ThreadRecord {
            id: id.to_string(),
            thread: format!("thread/{id}"),
            target_thread: None,
            parent_thread: None,
            mode: crate::ThreadMode::Materialized,
            state,
            base_state: StateId::from_bytes([1; 32]).to_string_full(),
            base_root: StateId::from_bytes([2; 32]).to_string_full(),
            current_state: None,
            merged_state: None,
            task: None,
            changed_paths: vec![],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: crate::ThreadFreshness::Current,
            verification_summary: Default::default(),
            confidence_summary: Default::default(),
            integration_policy_result: Default::default(),
            created_at: now,
            updated_at: now,
            ephemeral: marker,
            auto: false,
            shared_target_dir: None,
        }
    }

    #[test]
    fn expired_thread_collapses() {
        let created = Utc::now() - Duration::hours(2);
        let marker = EphemeralMarker {
            ttl_seconds: 60,
            created_at: created,
            auto_collapse: true,
        };
        let mut records = vec![make_record("t1", Some(marker), ThreadState::Active)];
        let collapsed = collapse_expired_ephemeral_threads(&mut records, Utc::now());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].thread_id, "t1");
        assert!(matches!(records[0].state, ThreadState::Abandoned));
    }

    #[test]
    fn non_expired_thread_unchanged() {
        let marker = EphemeralMarker {
            ttl_seconds: 3600,
            created_at: Utc::now(),
            auto_collapse: true,
        };
        let mut records = vec![make_record("t1", Some(marker), ThreadState::Active)];
        let collapsed = collapse_expired_ephemeral_threads(&mut records, Utc::now());
        assert!(collapsed.is_empty());
        assert!(matches!(records[0].state, ThreadState::Active));
    }

    #[test]
    fn auto_collapse_off_skips_collapse() {
        let created = Utc::now() - Duration::hours(2);
        let marker = EphemeralMarker {
            ttl_seconds: 60,
            created_at: created,
            auto_collapse: false,
        };
        let mut records = vec![make_record("t1", Some(marker), ThreadState::Active)];
        let collapsed = collapse_expired_ephemeral_threads(&mut records, Utc::now());
        assert!(collapsed.is_empty());
        assert!(matches!(records[0].state, ThreadState::Active));
    }

    #[test]
    fn non_ephemeral_thread_skipped() {
        let mut records = vec![make_record("t1", None, ThreadState::Active)];
        let collapsed = collapse_expired_ephemeral_threads(&mut records, Utc::now());
        assert!(collapsed.is_empty());
    }

    #[test]
    fn already_abandoned_thread_not_double_collapsed() {
        let created = Utc::now() - Duration::hours(2);
        let marker = EphemeralMarker {
            ttl_seconds: 60,
            created_at: created,
            auto_collapse: true,
        };
        let mut records = vec![make_record("t1", Some(marker), ThreadState::Abandoned)];
        let collapsed = collapse_expired_ephemeral_threads(&mut records, Utc::now());
        assert!(collapsed.is_empty());
    }

    #[test]
    fn merged_thread_not_collapsed() {
        let created = Utc::now() - Duration::hours(2);
        let marker = EphemeralMarker {
            ttl_seconds: 60,
            created_at: created,
            auto_collapse: true,
        };
        let mut records = vec![make_record("t1", Some(marker), ThreadState::Merged)];
        let collapsed = collapse_expired_ephemeral_threads(&mut records, Utc::now());
        assert!(collapsed.is_empty());
        assert!(matches!(records[0].state, ThreadState::Merged));
    }
}
