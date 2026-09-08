//! Immutable context revisions, sharing the same portable actor and scope as
//! discussions. The record ID addresses history; the operation ID names a revision.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CollaborationAnchor, CollaborationCodecError, CollaborationMetadata, DiscussionRecordId,
};
use crate::object::ContentHash;

pub const CONTEXT_FORMAT: &str = "heddle-context-revision-v2";
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
    pub tags: Vec<String>,
    pub supersedes: Option<Uuid>,
    pub extracted_from: Option<DiscussionRecordId>,
    pub occurred_at_ms: i64,
}
impl ContextRevision {
    pub fn encode(&self) -> Result<Vec<u8>, CollaborationCodecError> {
        self.metadata.validate()?;
        super::operation::validate_anchor(&self.anchor)?;
        if self.version != 2
            || self.id.is_nil()
            || self
                .supersedes
                .is_some_and(|id| id.is_nil() || id == self.id)
            || self.content.trim().is_empty()
            || self.content.len() > 256 * 1024
            || self.tags.len() > 128
            || self
                .tags
                .iter()
                .any(|tag| tag.trim().is_empty() || tag.len() > 512)
            || self.parents.len() > 128
            || self.parents.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(CollaborationCodecError::Invalid(
                "invalid or unbounded context revision".into(),
            ));
        }
        rmp_serde::to_vec_named(self).map_err(|e| CollaborationCodecError::Encoding(e.to_string()))
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
        Ok(ContentHash::compute_typed(CONTEXT_FORMAT, &self.encode()?))
    }
}
