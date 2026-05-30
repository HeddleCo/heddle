// SPDX-License-Identifier: Apache-2.0
//! The read-chokepoint reconciler seam (heddle#330 §2.2 "Reader model").
//!
//! All ten `RefManager` read methods funnel through `reconciled_load`, which
//! reconciles the raw-loaded ref value against the committed oplog tail. The
//! oplog lives in the `oplog` crate, which `refs` must NOT depend on, so the
//! fold is behind a [`RefReconciler`] trait **defined here** (over `refs`-owned
//! types) whose concrete oplog-backed impl is injected from the `repo`/`oplog`
//! layer via [`RefManager::with_reconciler`](super::RefManager::with_reconciler)
//! — the same dependency-inversion the write side uses.

use objects::{
    error::Result,
    object::{ChangeId, MarkerName, ThreadName},
};

use super::{Head, RefUpdate};

/// Whether a ref class reconciles within this checkout's `op_scope` (local) or
/// globally across all lanes (shared) — heddle#330 §2.2 r10.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefClass {
    /// `HEAD` + undo-recovery: live beside the per-worktree HEAD pointer, so a
    /// read folds only this worktree's lane (never lifting a sibling's HEAD).
    Local,
    /// thread, marker, remote-thread: one shared file per ref, so a read folds
    /// the full committed tail across all lanes (never missing a co-tenant's
    /// committed-but-unpublished write).
    Shared,
}

/// Which ref (or set of refs) a logical read wants. The single discriminator
/// `reconciled_load` dispatches on — a point read's raw sub-step reads one ref;
/// a list read's reads the summary set.
#[derive(Clone, Debug)]
pub enum LoadRequest {
    Head,
    Thread(ThreadName),
    Marker(MarkerName),
    UndoRecovery,
    RemoteThread { remote: String, thread: ThreadName },
    ThreadList,
    MarkerList,
    RemoteList,
    RemoteThreadList { remote: String },
}

impl LoadRequest {
    /// The ref class — local refs reconcile within `op_scope`, shared refs
    /// globally (heddle#330 §2.2 r10).
    pub fn ref_class(&self) -> RefClass {
        match self {
            LoadRequest::Head | LoadRequest::UndoRecovery => RefClass::Local,
            _ => RefClass::Shared,
        }
    }
}

/// The raw-loaded (and, after reconciliation, authoritative) value for a
/// [`LoadRequest`]. The variant matches the request shape.
#[derive(Clone, Debug)]
pub enum Loaded {
    Head(Head),
    Point(Option<ChangeId>),
    ThreadList(Vec<ThreadName>),
    MarkerList(Vec<MarkerName>),
    RemoteList(Vec<String>),
    RemoteThreadList(Vec<ThreadName>),
}

/// The result of a reconcile: the authoritative value for the request plus the
/// re-materialization set for **every** ref the lagged batches touched (lazily
/// re-published so the canonical cache catches up batch-atomically — heddle#330
/// §2.2 r8). The watermark may advance only after every ref of the class is
/// materialized, never after a partial single-ref reconcile.
///
/// `republish` carries the thread/marker/HEAD refs (expressible as
/// [`RefUpdate`]); `remote_updates` and `undo_recovery` carry the two classes
/// without a `RefUpdate` variant.
pub struct ReconcileOutcome {
    pub loaded: Loaded,
    pub republish: Vec<RefUpdate>,
    /// `(remote, thread, Some(state) | None == delete)` materializations.
    pub remote_updates: Vec<(String, ThreadName, Option<ChangeId>)>,
    /// New undo-recovery pointer to materialize, if a lagged batch set it.
    pub undo_recovery: Option<ChangeId>,
}

/// The write-side dual of [`RefReconciler`] (heddle#330 §2.2 "The write
/// chokepoint"): commits the caller's ref-carrying oplog record batch (phase 4)
/// before the canonical publish (phase 5), so no ref is published without a
/// preceding replayable record. The records cross the seam as opaque
/// rmp-serde-encoded bytes, so `refs` names no `oplog` type; the impl (in
/// `repo`) decodes and appends them. Injected via
/// [`RefManager::with_committer`](super::RefManager::with_committer).
pub trait RefCommitter: Send + Sync {
    /// Append the (opaque-encoded) ref-carrying `OpRecord` batch under the
    /// oplog write lock — phase 4, the commit point.
    fn commit_records(&self, encoded_records: &[Vec<u8>], scope: Option<&str>) -> Result<()>;
}

/// The oplog-backed fold, injected into `RefManager` from the `repo`/`oplog`
/// layer. Defined in `refs` over `refs`-owned types so `refs` keeps no `oplog`
/// dependency; the impl (which names `OpRecord`) lives in `repo`.
pub trait RefReconciler: Send + Sync {
    /// Current oplog generation — the monotonic `head_id`. The cheap O(1) gate:
    /// a read whose class watermark equals this returns the raw value with no
    /// tail scan.
    fn generation(&self) -> u64;

    /// Fold the committed oplog tail (scoped by the request's ref class) into
    /// `raw`, returning the authoritative value + the re-materialization set for
    /// **every** ref the lagged batches touched (batch-atomic). Only the
    /// committed entries newer than `since` (the class watermark) are folded, so
    /// the scan is bounded to what actually lags — never the whole history. Only
    /// invoked when the class watermark lags `generation()`.
    fn reconcile(&self, req: &LoadRequest, raw: Loaded, since: u64) -> Result<ReconcileOutcome>;
}
