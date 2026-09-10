//! Hosted landing is an executor attestation, never a human signature or a
//! transferable grant. Trust comes from the receiver's selected remote and
//! verified immutable Spool genesis, independently of incoming operation bytes.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ThreadGenesis, ThreadOperation, bounded, invalid};
use crate::{
    error::Result,
    object::{ContentHash, State, StateId},
};

pub const SPOOL_GENESIS_TRUST_FORMAT: &str = "heddle-spool-owner-genesis-executor-trust-v1";
pub const EXECUTION_FORMAT: &str = "heddle-hosted-integration-v1";
pub const MAX_EXECUTION_EVIDENCE: usize = 128;

/// This receipt is signed by the enclosing Thread operation's publisher. Source
/// originals stay in their own Thread; this fact evolves only the target graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedIntegration {
    pub version: u16,
    pub spool: Uuid,
    /// Digest of verified immutable Spool owner genesis, not its mutable owner.
    pub spool_genesis: ContentHash,
    pub executor: [u8; 32],
    pub source_thread: ContentHash,
    pub source_operation: ContentHash,
    pub source_revision: StateId,
    pub target_thread: ContentHash,
    pub expected_target_frontier: BTreeSet<ContentHash>,
    /// Exact canonical resulting State. Content installation is a separate,
    /// authorized closure transfer and never writes a checkout on receive.
    pub result: super::Capture,
    pub initiating_request_proof: ContentHash,
    pub review_policy_version: ContentHash,
    pub review_evidence: BTreeSet<ContentHash>,
    pub executed_at_ms: i64,
}
impl HostedIntegration {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
            || self.spool.is_nil()
            || self.source_thread == self.target_thread
            || self.expected_target_frontier.len() > 128
            || self.review_evidence.len() > MAX_EXECUTION_EVIDENCE
            || self.executed_at_ms < 0
        {
            return Err(invalid("invalid or unbounded hosted integration receipt"));
        }
        let state = State::decode_current_msgpack(&self.result.state)?;
        if state.encode_current_msgpack()? != self.result.state {
            return Err(invalid("non-canonical integration result"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let receipt: Self = rmp_serde::from_slice(bytes)?;
        if receipt.encode()? != bytes {
            return Err(invalid("non-canonical hosted integration receipt"));
        }
        Ok(receipt)
    }
    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(
            EXECUTION_FORMAT,
            &self.encode()?,
        ))
    }
    pub fn resulting_state(&self) -> Result<State> {
        State::decode_current_msgpack(&self.result.state)
    }
    /// Supply the independently authenticated original source operation.
    pub fn validate_source(&self, source: &ThreadOperation) -> Result<()> {
        if source.thread != self.source_thread
            || source.id()? != self.source_operation
            || source
                .source_state()?
                .is_none_or(|state| state.id() != self.source_revision)
        {
            return Err(invalid(
                "integration differs from original source operation",
            ));
        }
        if self.result.source_targets.is_none()
            && source
                .source_result()?
                .is_some_and(|result| result.source_targets.is_some())
        {
            return Err(invalid("integration drops source reference closure"));
        }
        Ok(())
    }
    pub(super) fn validate_operation(&self, operation: &ThreadOperation) -> Result<()> {
        if self.target_thread != operation.thread
            || self.executor != operation.publisher
            || self.expected_target_frontier != operation.parents
        {
            return Err(invalid(
                "integration receipt differs from signed executor, target or frontier",
            ));
        }
        Ok(())
    }
    pub(super) fn validate_parents(
        &self,
        genesis: &ThreadGenesis,
        parents: &[ThreadOperation],
    ) -> Result<()> {
        if self.spool.to_string() != genesis.spool {
            return Err(invalid("integration receipt belongs to another Spool"));
        }
        let state = self.resulting_state()?;
        // A fast-forward retains the original State identity; the trusted
        // executor attests its ancestry. A merge must explicitly retain both
        // the selected source revision and every target source parent.
        if state.id() != self.source_revision {
            let mut expected = BTreeSet::from([self.source_revision]);
            if parents.is_empty() {
                expected.insert(genesis.base);
            }
            for parent in parents {
                expected.insert(
                    parent
                        .source_state()?
                        .ok_or_else(|| invalid("integration parent is not source"))?
                        .id(),
                );
            }
            if state.parents.iter().copied().collect::<BTreeSet<_>>() != expected
                || state.parents.len() != expected.len()
            {
                return Err(invalid(
                    "integration result drops source or target ancestry",
                ));
            }
        }
        Ok(())
    }
}

/// Construct only from receiver-owned remote configuration and verified Spool
/// genesis. Copying these fields from an incoming receipt establishes no trust.
/// A hosted server also requires its persisted original execution receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedHostedExecutor {
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub executor: [u8; 32],
}
impl TrustedHostedExecutor {
    pub fn authorize(&self, operation: &ThreadOperation) -> Result<()> {
        let receipt = operation
            .hosted_execution_binding()?
            .ok_or_else(|| invalid("executor trust requires a hosted execution"))?;
        if receipt.spool != self.spool
            || receipt.spool_genesis != self.spool_genesis
            || receipt.executor != self.executor
        {
            return Err(invalid(
                "hosted execution has no independently trusted executor for this Spool genesis",
            ));
        }
        Ok(())
    }
}

/// Extracted after canonical receipt and enclosing operation binding checks.
/// It identifies an execution; receiving these bytes never establishes trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostedExecutionBinding {
    pub kind: HostedExecutionKind,
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub executor: [u8; 32],
    pub initiating_request_proof: ContentHash,
    pub review_policy_version: Option<ContentHash>,
    pub executed_at_ms: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedExecutionKind {
    Integration,
    Import,
}
impl ThreadOperation {
    pub fn hosted_execution_binding(&self) -> Result<Option<HostedExecutionBinding>> {
        if let Some(value) = self.integration()? {
            value.validate_operation(self)?;
            return Ok(Some(HostedExecutionBinding {
                kind: HostedExecutionKind::Integration,
                spool: value.spool,
                spool_genesis: value.spool_genesis,
                executor: value.executor,
                initiating_request_proof: value.initiating_request_proof,
                review_policy_version: Some(value.review_policy_version),
                executed_at_ms: value.executed_at_ms,
            }));
        }
        if let Some(value) = self.hosted_import()? {
            value.validate_operation(self)?;
            return Ok(Some(HostedExecutionBinding {
                kind: HostedExecutionKind::Import,
                spool: value.spool,
                spool_genesis: value.spool_genesis,
                executor: value.executor,
                initiating_request_proof: value.initiating_request_proof,
                review_policy_version: None,
                executed_at_ms: value.executed_at_ms,
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Attribution, Principal, Tree, thread_replication::ThreadOperationBody};

    fn fixture() -> (
        ThreadGenesis,
        ThreadOperation,
        HostedIntegration,
        ThreadOperation,
    ) {
        let genesis = ThreadGenesis {
            version: 1,
            spool: Uuid::from_u128(7).to_string(),
            parent: None,
            base: StateId::from_bytes([1; 32]),
            name: "target".into(),
            intent: "hosted landing".into(),
            owner: crate::object::thread_replication::GenesisOwner::Account(Uuid::from_u128(12)),
            creator: [2; 32],
            nonce: vec![3; 16],
        };
        let target = State::new_snapshot(
            Tree::new().hash(),
            vec![genesis.base],
            Attribution::human(Principal::new("target", "target@example.test")),
        );
        let target_operation = ThreadOperation {
            version: 1,
            thread: genesis.id().expect("Thread"),
            parents: BTreeSet::new(),
            publisher: [2; 32],
            body: ThreadOperationBody::Capture(
                crate::object::thread_replication::AuthoredCapture::local(
                    target.encode_current_msgpack().expect("target").into(),
                ),
            ),
        };
        let source = State::new_snapshot(
            Tree::new().hash(),
            vec![target.id()],
            Attribution::human(Principal::new("source", "source@example.test")),
        );
        let receipt = HostedIntegration {
            version: 1,
            spool: Uuid::from_u128(7),
            spool_genesis: ContentHash::from_bytes([4; 32]),
            executor: [5; 32],
            source_thread: ContentHash::from_bytes([6; 32]),
            source_operation: ContentHash::from_bytes([7; 32]),
            source_revision: source.id(),
            target_thread: genesis.id().expect("Thread"),
            expected_target_frontier: BTreeSet::from([target_operation.id().expect("parent")]),
            result: source.encode_current_msgpack().expect("source").into(),
            initiating_request_proof: ContentHash::from_bytes([8; 32]),
            review_policy_version: ContentHash::from_bytes([9; 32]),
            review_evidence: BTreeSet::from([ContentHash::from_bytes([10; 32])]),
            executed_at_ms: 100,
        };
        let operation = ThreadOperation {
            version: 1,
            thread: receipt.target_thread,
            parents: receipt.expected_target_frontier.clone(),
            publisher: receipt.executor,
            body: ThreadOperationBody::Integration(receipt.encode().expect("receipt")),
        };
        (genesis, target_operation, receipt, operation)
    }
    #[test]
    fn integration_is_bound_to_executor_target_frontier_and_scope() {
        let (genesis, parent, receipt, operation) = fixture();
        operation
            .validate_parents(&genesis, std::slice::from_ref(&parent))
            .expect("valid integration");
        assert_eq!(
            ThreadOperation::decode(&operation.encode().expect("encode")).expect("canonical"),
            operation
        );
        let mut changed = operation.clone();
        changed.publisher = [99; 32];
        assert!(changed.encode().is_err(), "receipt cannot change executor");
        changed = operation.clone();
        changed.parents.clear();
        assert!(
            changed.encode().is_err(),
            "receipt cannot change target frontier"
        );
        changed = operation.clone();
        changed.thread = ContentHash::from_bytes([99; 32]);
        assert!(
            changed.encode().is_err(),
            "receipt cannot change target Thread"
        );
        let mut wrong_scope = receipt;
        wrong_scope.spool = Uuid::from_u128(99);
        changed = operation;
        changed.body =
            ThreadOperationBody::Integration(wrong_scope.encode().expect("structural receipt"));
        assert!(
            changed.validate_parents(&genesis, &[parent]).is_err(),
            "receipt cannot change immutable Spool scope"
        );
    }
    #[test]
    fn integration_source_evolution_preserves_merge_parents_and_accepts_later_captures() {
        let (genesis, parent, mut receipt, mut operation) = fixture();
        let target = parent.source_state().expect("source").expect("target");
        let merge = State::new_snapshot(
            Tree::new().hash(),
            vec![target.id(), receipt.source_revision],
            Attribution::human(Principal::new("executor", "weft@example.test")),
        );
        receipt.result = merge.encode_current_msgpack().expect("merge").into();
        operation.body = ThreadOperationBody::Integration(receipt.encode().expect("receipt"));
        operation
            .validate_parents(&genesis, std::slice::from_ref(&parent))
            .expect("explicit merge ancestry");
        let after = State::new_snapshot(
            Tree::new().hash(),
            vec![merge.id()],
            Attribution::human(Principal::new("agent", "agent@example.test")),
        );
        let capture = ThreadOperation {
            version: 1,
            thread: operation.thread,
            parents: BTreeSet::from([operation.id().expect("integration")]),
            publisher: [3; 32],
            body: ThreadOperationBody::Capture(
                crate::object::thread_replication::AuthoredCapture::local(
                    after.encode_current_msgpack().expect("capture").into(),
                ),
            ),
        };
        capture
            .validate_parents(&genesis, std::slice::from_ref(&operation))
            .expect("capture after hosted integration");
        let dropped = State::new_snapshot(
            Tree::new().hash(),
            vec![target.id()],
            Attribution::human(Principal::new("executor", "weft@example.test")),
        );
        receipt.result = dropped
            .encode_current_msgpack()
            .expect("dropped ancestry")
            .into();
        operation.body = ThreadOperationBody::Integration(receipt.encode().expect("receipt"));
        assert!(
            operation.validate_parents(&genesis, &[parent]).is_err(),
            "merge cannot drop source ancestry"
        );
    }
    #[test]
    fn integration_signature_identity_does_not_create_executor_trust() {
        let (_, _, _, operation) = fixture();
        let trust = TrustedHostedExecutor {
            spool: Uuid::from_u128(7),
            spool_genesis: ContentHash::from_bytes([4; 32]),
            executor: [5; 32],
        };
        trust
            .authorize(&operation)
            .expect("independent selected remote pin");
        let mut different = trust.clone();
        different.executor = [9; 32];
        assert!(
            different.authorize(&operation).is_err(),
            "different endpoint is not user authority"
        );
        different = trust.clone();
        different.spool_genesis = ContentHash::from_bytes([9; 32]);
        assert!(
            different.authorize(&operation).is_err(),
            "same UUID does not replace immutable genesis"
        );
        different = trust;
        different.spool = Uuid::from_u128(9);
        assert!(
            different.authorize(&operation).is_err(),
            "executor trust is scoped to one Spool"
        );
    }
}
