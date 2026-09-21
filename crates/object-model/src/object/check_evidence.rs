//! Immutable check results and explicit policy-bound acknowledgements. A valid
//! signature proves authorship; admitting hosts independently verify authority.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::thread_replication::metadata::AUTHORITY_FORMAT;
use crate::{
    error::{HeddleError, Result},
    object::{CollaborationActor, ContentHash, StateId, collaboration::CanonicalBody},
};

pub const EVIDENCE_FORMAT: &str = "heddle-check-evidence-v2";
pub const ACKNOWLEDGEMENT_FORMAT: &str = "heddle-check-acknowledgement-v1";
pub const MAX_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckAuthor {
    pub actor: CollaborationActor,
    pub publisher: [u8; 32],
    pub authority_digest: ContentHash,
    pub authority_envelope: Vec<u8>,
}
impl CheckAuthor {
    fn validate(&self) -> Result<()> {
        if self.actor.principal_id.is_nil()
            || self.publisher == [0; 32]
            || self
                .actor
                .agent_id
                .as_ref()
                .is_some_and(|id| !valid_text(id, 256, false))
            || self.authority_envelope.is_empty()
            || self.authority_envelope.len() > 64 * 1024
            || ContentHash::compute_typed(AUTHORITY_FORMAT, &self.authority_envelope)
                != self.authority_digest
        {
            return Err(invalid("invalid check author authority binding"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEvidence {
    pub version: u16,
    pub id: Uuid,
    pub spool: Uuid,
    /// Original access scope; identical source in another Thread grants no access.
    pub thread: ContentHash,
    pub revision: StateId,
    pub check: String,
    pub outcome: CheckOutcome,
    pub detail: String,
    /// Independently authorized retained artifacts; a reference is no grant.
    pub artifacts: Vec<Uuid>,
    /// Explicit prior results from this same actor/check/revision; arrival time
    /// never decides which retry supersedes another result.
    pub supersedes: Vec<Uuid>,
    pub author: CheckAuthor,
    pub completed_at_ms: i64,
    /// Canonical MessagePack from the last successful [`Self::encode`] or [`Self::decode`].
    /// Not serialized. Struct literals use [`Default::default`].
    #[serde(skip)]
    pub canonical_body: CanonicalBody,
}
impl CheckEvidence {
    fn encode_fields(&self) -> Result<Vec<u8>> {
        self.author.validate()?;
        if self.version != 2
            || self.id.is_nil()
            || self.spool.is_nil()
            || self.completed_at_ms < 0
            || !valid_text(&self.check, 512, false)
            || !valid_text(&self.detail, 32 * 1024, true)
            || self.artifacts.len() > 64
            || self.artifacts.iter().any(Uuid::is_nil)
            || self.artifacts.windows(2).any(|ids| ids[0] >= ids[1])
            || self.supersedes.len() > 32
            || self
                .supersedes
                .iter()
                .any(|id| id.is_nil() || *id == self.id)
            || self.supersedes.windows(2).any(|ids| ids[0] >= ids[1])
        {
            return Err(invalid("invalid check evidence"));
        }
        bounded_encode(self)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.canonical_body.clear();
        let bytes = self.encode_fields()?;
        self.canonical_body.store(bytes.clone());
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bound(bytes)?;
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical check evidence"));
        }
        Ok(value)
    }
    pub fn id(&self) -> Result<ContentHash> {
        if let Some(bytes) = self.canonical_body.cloned() {
            CanonicalBody::debug_matches(&bytes, || self.encode_fields());
            return Ok(ContentHash::compute_typed(EVIDENCE_FORMAT, &bytes));
        }
        Ok(ContentHash::compute_typed(EVIDENCE_FORMAT, &self.encode()?))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckAcknowledgement {
    pub version: u16,
    pub spool: Uuid,
    pub evidence: Uuid,
    /// Exact accepted evidence content, preventing a mutable ID substitution.
    pub evidence_digest: ContentHash,
    pub revision: StateId,
    pub policy_version: ContentHash,
    pub author: CheckAuthor,
    pub client_operation_id: Uuid,
    pub occurred_at_ms: i64,
}
impl CheckAcknowledgement {
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.author.validate()?;
        if self.version != 1
            || self.spool.is_nil()
            || self.evidence.is_nil()
            || self.client_operation_id.is_nil()
            || self.occurred_at_ms < 0
        {
            return Err(invalid("invalid check acknowledgement"));
        }
        bounded_encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bound(bytes)?;
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical check acknowledgement"));
        }
        Ok(value)
    }
}
fn bounded_encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = rmp_serde::to_vec_named(value)?;
    bound(&bytes)?;
    Ok(bytes)
}
fn bound(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(invalid("check record exceeds byte bounds"));
    }
    Ok(())
}
fn valid_text(value: &str, max: usize, empty: bool) -> bool {
    (empty || !value.trim().is_empty()) && value.len() <= max && !value.contains('\0')
}
fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::object::{
        CollaborationActor, ContentHash, StateId, thread_replication::metadata::AUTHORITY_FORMAT,
    };

    fn sample() -> CheckEvidence {
        let envelope = b"independently verified authority".to_vec();
        CheckEvidence {
            version: 2,
            id: Uuid::from_u128(1),
            spool: Uuid::from_u128(2),
            thread: ContentHash::from_bytes([7; 32]),
            revision: StateId::from_bytes([3; 32]),
            check: "unit-tests".into(),
            outcome: CheckOutcome::Passed,
            detail: "42 passed".into(),
            artifacts: Vec::new(),
            supersedes: Vec::new(),
            author: CheckAuthor {
                actor: CollaborationActor {
                    principal_id: Uuid::from_u128(4),
                    agent_id: None,
                },
                publisher: [9; 32],
                authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
                authority_envelope: envelope,
            },
            completed_at_ms: 1000,
            canonical_body: Default::default(),
        }
    }

    #[test]
    fn id_matches_reencode_of_canonical_body() {
        let evidence = sample();
        let bytes = evidence.encode().expect("canonical evidence");
        let old = ContentHash::compute_typed(EVIDENCE_FORMAT, &bytes);
        assert_eq!(evidence.id().expect("fresh id"), old);
        assert_eq!(evidence.id().expect("cached id"), old);

        let decoded = CheckEvidence::decode(&bytes).expect("decode");
        assert_eq!(decoded, evidence);
        assert_eq!(decoded.id().expect("decoded id"), old);
        assert_eq!(
            decoded.id().expect("decoded id"),
            ContentHash::compute_typed(EVIDENCE_FORMAT, &decoded.encode().expect("re-encode"))
        );

        let mut changed = evidence.clone();
        changed.outcome = CheckOutcome::Failed;
        let changed_bytes = changed.encode().expect("changed evidence");
        let changed_id = changed.id().expect("changed id");
        assert_ne!(changed_id, old);
        assert_eq!(
            changed_id,
            ContentHash::compute_typed(EVIDENCE_FORMAT, &changed_bytes)
        );
    }
}
