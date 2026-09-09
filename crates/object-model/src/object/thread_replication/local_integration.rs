//! Authored device integration. This is ordinary source work, never a hosted
//! executor's policy approval. Admission separately authorizes both Threads and
//! verifies the referenced original source operation before storing this record.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{ThreadGenesis, ThreadOperation, bounded, invalid};
use crate::{
    error::Result,
    object::{ContentHash, State, StateId, VisibilityTier},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalIntegration {
    pub version: u16,
    pub spool: uuid::Uuid,
    pub device: [u8; 32],
    pub source_thread: ContentHash,
    pub source_operation: ContentHash,
    pub source_revision: StateId,
    pub target_thread: ContentHash,
    pub expected_target_frontier: BTreeSet<ContentHash>,
    pub result: super::Capture,
    pub result_visibility: VisibilityTier,
    pub initiating_request_proof: ContentHash,
    pub local_policy_version: ContentHash,
    pub executed_at_ms: i64,
}
impl LocalIntegration {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
            || self.spool.is_nil()
            || self.source_thread == self.target_thread
            || self.expected_target_frontier.len() > 128
            || self.executed_at_ms < 0
        {
            return Err(invalid("invalid local integration receipt"));
        }
        let state = State::decode_current_msgpack(&self.result.state)?;
        if state.encode_current_msgpack()? != self.result.state {
            return Err(invalid("non-canonical local integration State"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let receipt: Self = rmp_serde::from_slice(bytes)?;
        if receipt.encode()? != bytes {
            return Err(invalid("non-canonical local integration receipt"));
        }
        Ok(receipt)
    }
    pub fn resulting_state(&self) -> Result<State> {
        State::decode_current_msgpack(&self.result.state)
    }
    /// Supply the independently authenticated original record from the named
    /// source Thread, never a record selected solely by a claimed revision hash.
    pub fn validate_source(&self, source: &ThreadOperation) -> Result<()> {
        if self.result.source_targets.is_none()
            && source
                .source_result()?
                .is_some_and(|result| result.source_targets.is_some())
        {
            return Err(invalid("integration drops source reference closure"));
        }
        if source.thread != self.source_thread
            || source.id()? != self.source_operation
            || source
                .source_state()?
                .is_none_or(|state| state.id() != self.source_revision)
        {
            return Err(invalid(
                "local integration source differs from original operation",
            ));
        }
        Ok(())
    }
    pub(super) fn validate_operation(&self, operation: &ThreadOperation) -> Result<()> {
        if operation.thread != self.target_thread
            || operation.publisher != self.device
            || operation.parents != self.expected_target_frontier
        {
            return Err(invalid(
                "local integration changes device, target or observed frontier",
            ));
        }
        Ok(())
    }
    pub(super) fn validate_parents(
        &self,
        genesis: &ThreadGenesis,
        parents: &[ThreadOperation],
    ) -> Result<()> {
        if genesis.spool != self.spool.to_string() {
            return Err(invalid("local integration belongs to another Spool"));
        }
        let state = self.resulting_state()?;
        let mut expected = BTreeSet::from([self.source_revision]);
        if parents.is_empty() {
            expected.insert(genesis.base);
        }
        for parent in parents {
            expected.insert(
                parent
                    .source_state()?
                    .ok_or_else(|| invalid("local integration parent is not source"))?
                    .id(),
            );
        }
        // Even a same-tree local landing creates an attributed merge record;
        // its cross-Thread ancestry is explicit rather than executor-attested.
        if state.parents.iter().copied().collect::<BTreeSet<_>>() != expected
            || state.parents.len() != expected.len()
        {
            return Err(invalid("local integration drops source or target ancestry"));
        }
        Ok(())
    }
}

/// Intersection of representable disclosure sets. Different named audiences
/// require an explicit policy decision, never a rank-only lateral downgrade.
pub fn intersect_visibility(
    left: &VisibilityTier,
    right: &VisibilityTier,
) -> Result<VisibilityTier> {
    use VisibilityTier::*;
    if left == right {
        return Ok(left.clone());
    }
    match (left, right) {
        (Public, other) | (other, Public) => Ok(other.clone()),
        (Internal, other) | (other, Internal) => Ok(other.clone()),
        (Private { scope_label: a }, Restricted { scope_label: b })
        | (Restricted { scope_label: b }, Private { scope_label: a })
            if a == b =>
        {
            Ok(Private {
                scope_label: a.clone(),
            })
        }
        _ => Err(invalid("integration combines incomparable named audiences")),
    }
}
