// SPDX-License-Identifier: Apache-2.0
//! Structured query over the operation log.

use std::{collections::BTreeMap, path::Path};

use chrono::TimeZone;
use objects::{
    error::Result,
    object::{OperationId, StateId},
};
use oplog::{OpEntry, OpLog, OpLogBackend, OpRecord};
use refs::refs::{IndexedOperation, OperationLogIndex, OperationLogQuery};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract,
    schema_for_report,
};

/// Query filters for the operation log facade.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryRequest {
    pub actor: String,
    pub symbol: String,
    pub signal_kind: String,
    pub thread: String,
    pub verbs: Vec<String>,
    pub since_secs: i64,
    pub until_secs: i64,
    pub limit: u32,
    pub include_checkpoints: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct QueryReport {
    pub output_kind: &'static str,
    pub hits: Vec<QueryHit>,
}

impl QueryReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "query",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "query",
        }),
        schema: schema_for_report::<QueryReport>,
    };
}

impl HeddleReport for QueryReport {
    const CONTRACT: ReportContract = QueryReport::CONTRACT;
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct QueryHit {
    pub seq: u64,
    pub timestamp_secs: i64,
    pub verb: String,
    pub actor_email: String,
    pub operation_id: Option<String>,
    pub thread: Option<String>,
    pub symbols: Vec<String>,
    pub signal_kinds: Vec<String>,
    pub state_id: Option<String>,
}

/// Query is an operator-facing inspection command, so it should answer from
/// the live oplog even before the rebuildable index sidecar has been warmed.
/// Keep the scan bounded; long-tail history can use the index once populated.
const OPLOG_FALLBACK_SCAN_WINDOW: usize = 100_000;

pub fn query(ctx: &ExecutionContext, req: QueryRequest) -> Result<QueryReport> {
    let repo = ctx.require_repo()?;
    let q = build_query(&req);
    let hits = query_combined(repo.heddle_dir(), &q)?;
    Ok(QueryReport {
        output_kind: "query",
        hits: hits.into_iter().map(hit_to_report).collect(),
    })
}

fn build_query(req: &QueryRequest) -> OperationLogQuery {
    let mut q = OperationLogQuery {
        actor: (!req.actor.is_empty()).then(|| req.actor.clone()),
        symbol: (!req.symbol.is_empty()).then(|| req.symbol.clone()),
        signal_kind: (!req.signal_kind.is_empty()).then(|| req.signal_kind.clone()),
        thread: (!req.thread.is_empty()).then(|| req.thread.clone()),
        verbs: (!req.verbs.is_empty()).then(|| req.verbs.clone()),
        since: parse_unix_secs(req.since_secs),
        until: parse_unix_secs(req.until_secs),
        limit: (req.limit > 0).then_some(req.limit as usize),
    };
    if !req.include_checkpoints && q.verbs.is_none() {
        q.verbs = Some(
            OpRecord::verbs(false)
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }
    q
}

fn parse_unix_secs(secs: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if secs == 0 {
        return None;
    }
    chrono::Utc.timestamp_opt(secs, 0).single()
}

fn query_combined(heddle_dir: &Path, query: &OperationLogQuery) -> Result<Vec<IndexedOperation>> {
    let index = OperationLogIndex::new(heddle_dir);
    let mut unbounded = query.clone();
    unbounded.limit = None;

    let mut by_seq = BTreeMap::new();
    for hit in index.query(&unbounded)? {
        by_seq.insert(hit.seq, hit);
    }

    if unbounded.symbol.is_none() && unbounded.signal_kind.is_none() {
        for hit in query_oplog_fallback(heddle_dir, &unbounded)? {
            by_seq.entry(hit.seq).or_insert(hit);
        }
    }

    let mut hits: Vec<_> = by_seq.into_values().collect();
    hits.sort_by_key(|hit| hit.seq);
    if let Some(limit) = query.limit {
        hits.truncate(limit);
    }
    Ok(hits)
}

fn query_oplog_fallback(
    heddle_dir: &Path,
    query: &OperationLogQuery,
) -> Result<Vec<IndexedOperation>> {
    let log = OpLog::new_unattributed(heddle_dir);
    let mut entries = log.recent(OPLOG_FALLBACK_SCAN_WINDOW)?;
    entries.reverse();
    let mut hits = Vec::new();
    for entry in entries {
        let hit = indexed_from_oplog_entry(&entry);
        if indexed_operation_matches(&hit, query) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn indexed_from_oplog_entry(entry: &OpEntry) -> IndexedOperation {
    IndexedOperation {
        seq: entry.id,
        timestamp_secs: entry.timestamp.timestamp(),
        verb: entry.operation.verb().to_string(),
        actor_email: entry.actor.email.clone(),
        operation_id: entry.operation_id,
        thread: thread_for(&entry.operation),
        symbols: Vec::new(),
        signal_kinds: Vec::new(),
        state_id: primary_state_id(&entry.operation),
    }
}

fn indexed_operation_matches(hit: &IndexedOperation, query: &OperationLogQuery) -> bool {
    if let Some(actor) = &query.actor
        && &hit.actor_email != actor
    {
        return false;
    }
    if let Some(symbol) = &query.symbol
        && !hit.symbols.iter().any(|candidate| candidate == symbol)
    {
        return false;
    }
    if let Some(kind) = &query.signal_kind
        && !hit.signal_kinds.iter().any(|candidate| candidate == kind)
    {
        return false;
    }
    if let Some(thread) = &query.thread
        && hit.thread.as_deref() != Some(thread.as_str())
    {
        return false;
    }
    if let Some(verbs) = &query.verbs
        && !verbs.iter().any(|verb| verb == &hit.verb)
    {
        return false;
    }
    let timestamp = hit.timestamp();
    if let Some(start) = query.since
        && timestamp < start
    {
        return false;
    }
    if let Some(end) = query.until
        && timestamp > end
    {
        return false;
    }
    true
}

fn hit_to_report(hit: IndexedOperation) -> QueryHit {
    QueryHit {
        seq: hit.seq,
        timestamp_secs: hit.timestamp_secs,
        verb: hit.verb,
        actor_email: hit.actor_email,
        operation_id: hit.operation_id.map(operation_id_to_string),
        thread: hit.thread,
        symbols: hit.symbols,
        signal_kinds: hit.signal_kinds,
        state_id: hit.state_id.map(|id| id.to_string_full()),
    }
}

fn operation_id_to_string(id: OperationId) -> String {
    id.to_string()
}

fn thread_for(op: &OpRecord) -> Option<String> {
    match op {
        OpRecord::Snapshot { thread, .. } => thread.clone(),
        OpRecord::ThreadCreate { name, .. } => Some(name.clone()),
        OpRecord::ThreadDelete { name, .. } => Some(name.clone()),
        OpRecord::ThreadUpdate { name, .. } => Some(name.clone()),
        OpRecord::MarkerCreate { name, .. } => Some(name.clone()),
        OpRecord::MarkerDelete { name, .. } => Some(name.clone()),
        OpRecord::Checkpoint { thread, .. } => thread.clone(),
        OpRecord::EphemeralThreadCollapse { thread, .. } => Some(thread.clone()),
        OpRecord::FastForward { target_thread, .. } => Some(target_thread.clone()),
        OpRecord::GitCheckpoint { branch, .. } => Some(branch.clone()),
        OpRecord::RemoteThreadUpdate { thread, .. }
        | OpRecord::RemoteThreadDelete { thread, .. } => Some(thread.clone()),
        OpRecord::Goto { .. }
        | OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Redact { .. }
        | OpRecord::UndoRecoveryUpdate { .. }
        | OpRecord::StateVisibilitySet { .. }
        | OpRecord::StateVisibilityPromote { .. }
        | OpRecord::Purge { .. } => None,
    }
}

fn primary_state_id(op: &OpRecord) -> Option<StateId> {
    match op {
        OpRecord::Snapshot { new_state, .. } => Some(*new_state),
        OpRecord::Goto { target, .. } => Some(*target),
        OpRecord::ThreadCreate { state, .. } => Some(*state),
        OpRecord::ThreadDelete { state, .. } => Some(*state),
        OpRecord::ThreadUpdate { new_state, .. } => Some(*new_state),
        OpRecord::Fork { new_state, .. } => Some(*new_state),
        OpRecord::Collapse { result, .. } => Some(*result),
        OpRecord::MarkerCreate { state, .. } => Some(*state),
        OpRecord::MarkerDelete { state, .. } => Some(*state),
        OpRecord::Checkpoint { state, .. } => Some(*state),
        OpRecord::GitCheckpoint { state, .. } => Some(*state),
        OpRecord::EphemeralThreadCollapse { final_state, .. } => Some(*final_state),
        OpRecord::Redact { state, .. } => Some(*state),
        OpRecord::StateVisibilitySet { state, .. }
        | OpRecord::StateVisibilityPromote { state, .. } => Some(*state),
        OpRecord::RemoteThreadUpdate { state, .. } | OpRecord::RemoteThreadDelete { state, .. } => {
            Some(*state)
        }
        OpRecord::UndoRecoveryUpdate { state } => Some(*state),
        OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Purge { .. }
        | OpRecord::FastForward { .. } => None,
    }
}
