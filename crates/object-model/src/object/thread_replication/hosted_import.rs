//! Hosted import is an executor attestation of provider provenance. The original
//! creator-signed genesis and imported Git authorship are never replaced by a
//! claim that the initiating human signed a future source capture.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Capture, ThreadGenesis, ThreadOperation, bounded, invalid};
use crate::{
    error::Result,
    object::{ContentHash, State},
};

pub const HOSTED_IMPORT_FORMAT: &str = "heddle-hosted-import-v1";

/// Stable synthetic pre-history for browser imports into a new Spool. This is
/// system initialization, never an assertion of human capture authorship.
/// Local repositories may retain their own random initial seed instead.
pub fn synthetic_initial_base() -> Result<State> {
    use crate::object::{Attribution, ChangeId, Principal, Tree};
    let mut state = State::new_refresh_of(
        Tree::new().hash(),
        Vec::new(),
        Attribution::human(Principal::new("Heddle", "init@heddle")),
        ChangeId::from_bytes(*b"heddle-seed-v2!!"),
    );
    state.created_at = chrono::DateTime::UNIX_EPOCH;
    // Restore the derived cached ID after fixing the canonical creation time.
    State::decode_current_msgpack(&state.encode_current_msgpack()?)
}

/// Validate a portable initial base supplied with the creator-signed genesis.
/// Its random logical identity and creation time are retained exactly; only the
/// existing synthetic Heddle seed shape is accepted, so no hidden object closure
/// can be smuggled into the empty-Spool bootstrap.
pub fn initial_base_state(genesis: &ThreadGenesis, bytes: &[u8]) -> Result<State> {
    use crate::object::{Attribution, Principal, Tree};
    if bytes.is_empty() || bytes.len() > 4096 {
        return Err(invalid("initial Thread base exceeds bootstrap bound"));
    }
    let state = State::decode_current_msgpack(bytes)?;
    let mut expected = State::new_refresh_of(
        Tree::new().hash(),
        Vec::new(),
        Attribution::human(Principal::new("Heddle", "init@heddle")),
        state.change_id,
    );
    expected.created_at = state.created_at;
    if state.id() != genesis.base || expected.encode_current_msgpack()? != bytes {
        return Err(invalid(
            "initial Thread base differs from signed empty seed",
        ));
    }
    Ok(state)
}

/// Git's algorithm is part of identity; a SHA-256 object never truncates to SHA-1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedCommit {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedImport {
    pub version: u16,
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub executor: [u8; 32],
    pub target_thread: ContentHash,
    pub expected_target_frontier: BTreeSet<ContentHash>,
    /// Thread source ancestry is independent of the retained Git history. The
    /// executor preserves Git attribution and records its source commit below.
    pub result: Capture,
    pub provider: String,
    /// Stable provider repository identity or credential-free public Git locator.
    pub provider_repository_id: String,
    pub source_commit: ImportedCommit,
    pub initiating_request_proof: ContentHash,
    pub executed_at_ms: i64,
}
impl HostedImport {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
            || self.spool.is_nil()
            || self.expected_target_frontier.len() > 128
            || self.executed_at_ms < 0
            || !matches!(self.provider.as_str(), "github" | "git")
            || self.provider_repository_id.is_empty()
            || self.provider_repository_id.len() > 4096
            || self.provider_repository_id.chars().any(char::is_control)
        {
            return Err(invalid("invalid or unbounded hosted import receipt"));
        }
        let state = self.resulting_state()?;
        if state.encode_current_msgpack()? != self.result.state {
            return Err(invalid("non-canonical hosted import capture"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("non-canonical hosted import receipt"));
        }
        Ok(value)
    }
    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(
            HOSTED_IMPORT_FORMAT,
            &self.encode()?,
        ))
    }
    pub fn resulting_state(&self) -> Result<State> {
        State::decode_current_msgpack(&self.result.state)
    }
    pub(super) fn validate_operation(&self, operation: &ThreadOperation) -> Result<()> {
        if self.target_thread != operation.thread
            || self.executor != operation.publisher
            || self.expected_target_frontier != operation.parents
        {
            return Err(invalid(
                "hosted import differs from signed executor, Thread or frontier",
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
            return Err(invalid("hosted import belongs to another Spool"));
        }
        let state = self.resulting_state()?;
        let mut expected = BTreeSet::new();
        if parents.is_empty() {
            expected.insert(genesis.base);
        }
        for parent in parents {
            expected.insert(
                parent
                    .source_state()?
                    .ok_or_else(|| invalid("hosted import parent is not source"))?
                    .id(),
            );
        }
        if state.parents.iter().copied().collect::<BTreeSet<_>>() != expected
            || state.parents.len() != expected.len()
        {
            return Err(invalid(
                "hosted import capture drops or invents Thread ancestry",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{
        Attribution, Principal, StateId, Tree, thread_replication::ThreadOperationBody,
    };
    fn fixture() -> (ThreadGenesis, HostedImport, ThreadOperation) {
        let genesis = ThreadGenesis {
            version: 1,
            spool: Uuid::from_u128(7).to_string(),
            parent: None,
            base: StateId::from_bytes([1; 32]),
            name: "import".into(),
            intent: "import Git history".into(),
            owner: crate::object::thread_replication::GenesisOwner::Account(Uuid::from_u128(12)),
            creator: [2; 32],
            nonce: vec![3; 16],
        };
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![genesis.base],
            Attribution::human(Principal::new("Git author", "git@example.test")),
        );
        let receipt = HostedImport {
            version: 1,
            spool: Uuid::from_u128(7),
            spool_genesis: ContentHash::from_bytes([4; 32]),
            executor: [5; 32],
            target_thread: genesis.id().expect("Thread"),
            expected_target_frontier: BTreeSet::new(),
            result: state.encode_current_msgpack().expect("source").into(),
            provider: "github".into(),
            provider_repository_id: "123".into(),
            source_commit: ImportedCommit::Sha1([6; 20]),
            initiating_request_proof: ContentHash::from_bytes([7; 32]),
            executed_at_ms: 100,
        };
        let operation = ThreadOperation {
            version: 1,
            thread: receipt.target_thread,
            parents: BTreeSet::new(),
            publisher: receipt.executor,
            body: ThreadOperationBody::HostedImport(receipt.encode().expect("receipt")),
        };
        (genesis, receipt, operation)
    }
    #[test]
    fn import_preserves_git_attribution_and_binds_executor_scope_and_ancestry() {
        let (genesis, receipt, operation) = fixture();
        operation.validate_parents(&genesis, &[]).expect("import");
        let decoded =
            ThreadOperation::decode(&operation.encode().expect("canonical")).expect("decode");
        assert_eq!(decoded, operation);
        assert_eq!(
            decoded
                .source_state()
                .expect("state")
                .expect("capture")
                .attribution
                .principal
                .name,
            b"Git author".to_vec()
        );
        assert_ne!(
            operation.publisher, genesis.creator,
            "executor does not impersonate creator"
        );
        let mut changed = operation.clone();
        changed.publisher = genesis.creator;
        assert!(changed.encode().is_err());
        changed = operation.clone();
        changed.parents.insert(ContentHash::from_bytes([99; 32]));
        assert!(changed.encode().is_err());
        let mut foreign = receipt.clone();
        foreign.spool = Uuid::from_u128(8);
        changed = operation.clone();
        changed.body = ThreadOperationBody::HostedImport(foreign.encode().expect("structural"));
        assert!(changed.validate_parents(&genesis, &[]).is_err());
        let mut wrong = receipt;
        let mut state = wrong.resulting_state().expect("capture");
        state.parents.clear();
        wrong.result.state = state.encode_current_msgpack().expect("wrong ancestry");
        changed = operation;
        changed.body = ThreadOperationBody::HostedImport(wrong.encode().expect("structural"));
        assert!(changed.validate_parents(&genesis, &[]).is_err());
    }
    #[test]
    fn later_capture_retains_imported_source_and_reference_frontier() {
        let (genesis, mut receipt, mut imported) = fixture();
        receipt.result.source_targets = Some(ContentHash::from_bytes([17; 32]));
        imported.body = ThreadOperationBody::HostedImport(receipt.encode().expect("receipt"));
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![receipt.resulting_state().expect("source").id()],
            Attribution::human(Principal::new("Human", "human@example.test")),
        );
        let mut capture = ThreadOperation {
            version: 1,
            thread: imported.thread,
            parents: BTreeSet::from([imported.id().expect("parent")]),
            publisher: genesis.creator,
            body: ThreadOperationBody::Capture(Capture {
                state: state.encode_current_msgpack().expect("capture"),
                source_targets: receipt.result.source_targets,
            }),
        };
        capture
            .validate_parents(&genesis, std::slice::from_ref(&imported))
            .expect("later capture");
        let ThreadOperationBody::Capture(result) = &mut capture.body else {
            panic!("capture")
        };
        result.source_targets = None;
        assert!(
            capture.validate_parents(&genesis, &[imported]).is_err(),
            "imported references cannot disappear"
        );
    }
    #[test]
    fn genesis_local_ownership_is_creator_bound_and_account_ownership_is_explicit() {
        use crate::object::thread_replication::GenesisOwner;
        let (mut genesis, _, _) = fixture();
        let account_id = genesis.id().expect("account-owned identity");
        genesis.owner = GenesisOwner::LocalKey(genesis.creator);
        let local_id = genesis.id().expect("local key needs no account");
        assert_ne!(account_id, local_id, "owner is part of immutable identity");
        genesis.owner = GenesisOwner::LocalKey([99; 32]);
        assert!(
            genesis.encode().is_err(),
            "local owner must sign its genesis"
        );
        genesis.owner = GenesisOwner::LocalKey([0; 32]);
        genesis.creator = [0; 32];
        assert!(genesis.encode().is_err(), "zero local key is not an owner");
        genesis.owner = GenesisOwner::Account(Uuid::nil());
        assert!(genesis.encode().is_err(), "nil account is not an owner");
    }
    #[test]
    fn import_initial_base_is_exact_bounded_empty_seed() {
        let (mut genesis, _, _) = fixture();
        let seed = State::new_snapshot(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new("Heddle", "init@heddle")),
        );
        let bytes = seed.encode_current_msgpack().expect("seed");
        genesis.base = seed.id();
        assert_eq!(
            initial_base_state(&genesis, &bytes)
                .expect("one-call bootstrap")
                .id(),
            seed.id()
        );
        let mut changed = seed.clone();
        changed.tree = ContentHash::from_bytes([88; 32]);
        genesis.base = changed.id();
        assert!(
            initial_base_state(
                &genesis,
                &changed.encode_current_msgpack().expect("changed")
            )
            .is_err(),
            "nonempty source needs authorized closure transfer"
        );
        changed = seed.clone();
        changed.provenance = Some(ContentHash::from_bytes([89; 32]));
        genesis.base = changed.id();
        assert!(
            initial_base_state(
                &genesis,
                &changed.encode_current_msgpack().expect("changed")
            )
            .is_err(),
            "seed cannot introduce another reference"
        );
        assert!(
            initial_base_state(&genesis, &bytes).is_err(),
            "base must match signed identity"
        );
    }
    #[test]
    fn synthetic_initial_base_is_stable_across_rust_and_browser() {
        let state = synthetic_initial_base().expect("synthetic seed");
        let bytes = state.encode_current_msgpack().expect("canonical seed");
        let expected = include_str!("../../../tests/fixtures/synthetic-initial-base-v2.txt");
        assert_eq!(
            format!(
                "canonical={}\nid={}\n",
                hex::encode(&bytes),
                hex::encode(state.id().as_bytes())
            ),
            expected
        );
        assert_eq!(
            bytes,
            synthetic_initial_base()
                .expect("repeat")
                .encode_current_msgpack()
                .expect("repeat bytes")
        );
        let (mut genesis, _, _) = fixture();
        genesis.base = state.id();
        initial_base_state(&genesis, &bytes).expect("accepted seed shape");
    }
}
