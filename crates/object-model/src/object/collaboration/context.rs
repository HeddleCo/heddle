//! Immutable context revisions, sharing the same portable actor and scope as
//! discussions. The record ID addresses history; the operation ID names a revision.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CollaborationAnchor, CollaborationCodecError, CollaborationMetadata, DiscussionRecordId,
    canonical_body::CanonicalBody,
};
use crate::object::{AnnotationKind, ContentHash, StateId};

pub const CONTEXT_FORMAT: &str = "heddle-context-revision-v2";

/// Evidence captured by the authoring context command. This remains inside the
/// signed native operation even when a hosted view does not project every
/// field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextProvenance {
    pub revision_id: String,
    pub kind: AnnotationKind,
    pub attribution: String,
    pub source_hash: Option<ContentHash>,
    pub created_at_state: Option<StateId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRevision {
    pub version: u16,
    pub id: Uuid,
    /// Original outer operation IDs, never mutable observation versions.
    pub parents: Vec<ContentHash>,
    pub metadata: CollaborationMetadata,
    pub anchor: CollaborationAnchor,
    pub content: String,
    pub tags: Vec<super::AnnotationTag>,
    pub supersedes: Option<Uuid>,
    pub extracted_from: Option<DiscussionRecordId>,
    pub occurred_at_ms: i64,
    /// Absent only on native records authored before provenance was carried in
    /// the signed body. New local replication must always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ContextProvenance>,
    /// Canonical MessagePack from the last successful [`Self::encode`] or [`Self::decode`].
    /// Not serialized. Struct literals use [`Default::default`].
    #[serde(skip)]
    pub canonical_body: CanonicalBody,
}
impl ContextRevision {
    fn encode_fields(&self) -> Result<Vec<u8>, CollaborationCodecError> {
        self.metadata.validate()?;
        super::validate_annotation_tags(&self.tags)?;
        super::operation::validate_anchor(&self.anchor)?;
        if self.version != 2
            || self.id.is_nil()
            || self
                .supersedes
                .is_some_and(|id| id.is_nil() || id == self.id)
            || self.content.trim().is_empty()
            || self.content.len() > 256 * 1024
            || self.parents.len() > 128
            || self.parents.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(CollaborationCodecError::Invalid(
                "invalid or unbounded context revision".into(),
            ));
        }
        rmp_serde::to_vec_named(self).map_err(|e| CollaborationCodecError::Encoding(e.to_string()))
    }
    pub fn encode(&self) -> Result<Vec<u8>, CollaborationCodecError> {
        self.canonical_body.clear();
        let bytes = self.encode_fields()?;
        self.canonical_body.store(bytes.clone());
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, CollaborationCodecError> {
        if bytes.len() > 512 * 1024 {
            return Err(CollaborationCodecError::Invalid(
                "context revision exceeds record bound".into(),
            ));
        }
        let record: Self = rmp_serde::from_slice(bytes)
            .map_err(|e| CollaborationCodecError::Decoding(e.to_string()))?;
        if record.encode()? != bytes {
            return Err(CollaborationCodecError::Invalid(
                "context revision is not canonical".into(),
            ));
        }
        Ok(record)
    }
    pub fn id(&self) -> Result<ContentHash, CollaborationCodecError> {
        if let Some(bytes) = self.canonical_body.cloned() {
            CanonicalBody::debug_matches(&bytes, || self.encode_fields());
            return Ok(ContentHash::compute_typed(CONTEXT_FORMAT, &bytes));
        }
        Ok(ContentHash::compute_typed(CONTEXT_FORMAT, &self.encode()?))
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::object::{
        CollaborationActor, CollaborationAnchor, CollaborationMetadata, CollaborationScope,
        ContentHash,
    };

    fn sample() -> ContextRevision {
        ContextRevision {
            version: 2,
            id: Uuid::from_u128(9),
            parents: Vec::new(),
            metadata: CollaborationMetadata {
                scope: CollaborationScope {
                    spool: Uuid::from_u128(1),
                    thread: None,
                },
                actor: CollaborationActor {
                    principal_id: Uuid::from_u128(2),
                    agent_id: None,
                },
                mentions: Vec::new(),
            },
            anchor: CollaborationAnchor::Repository,
            content: "rationale".into(),
            tags: Vec::new(),
            supersedes: None,
            extracted_from: None,
            occurred_at_ms: 10,
            provenance: None,
            canonical_body: Default::default(),
        }
    }

    #[test]
    fn id_matches_reencode_of_canonical_body() {
        let revision = sample();
        let bytes = revision.encode().expect("canonical revision");
        let old = ContentHash::compute_typed(CONTEXT_FORMAT, &bytes);
        assert_eq!(revision.id().expect("fresh id"), old);
        assert_eq!(revision.id().expect("cached id"), old);

        let decoded = ContextRevision::decode(&bytes).expect("decode");
        assert_eq!(decoded, revision);
        assert_eq!(decoded.id().expect("decoded id"), old);
        assert_eq!(
            decoded.id().expect("decoded id"),
            ContentHash::compute_typed(CONTEXT_FORMAT, &decoded.encode().expect("re-encode"))
        );

        let mut changed = revision.clone();
        changed.content = "other rationale".into();
        let changed_id = changed.id().expect("changed id");
        assert_ne!(changed_id, old);
        assert_eq!(
            changed_id,
            ContentHash::compute_typed(CONTEXT_FORMAT, &changed.encode().expect("changed"))
        );
    }
}
