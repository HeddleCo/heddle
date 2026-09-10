//! Exact source locations preserved across local and hosted collaboration.
use serde::{Deserialize, Serialize};

use super::CollaborationCodecError;
use crate::object::StateId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationRevision {
    State { state_id: StateId },
    GitCommit { oid: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationSourceAnchor {
    pub revision: CollaborationRevision,
    pub path: String,
    pub symbol_id: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    /// Original coordinates remain evidence. Without an explicit target this
    /// is an exact location, never an implicitly tracking reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<crate::object::source_target::SourceTargetReference>,
}
impl CollaborationSourceAnchor {
    pub(crate) fn validate(&self) -> Result<(), CollaborationCodecError> {
        if let Some(target) = &self.target {
            target
                .validate()
                .map_err(|error| CollaborationCodecError::Invalid(error.to_string()))?;
        }
        if let CollaborationRevision::GitCommit { oid } = &self.revision {
            if !matches!(oid.len(), 40 | 64)
                || !oid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(CollaborationCodecError::Invalid(
                    "source anchor requires an exact lowercase Git object ID".into(),
                ));
            }
        }
        if self.path.len() > 4096
            || self.symbol_id.len() > 4096
            || self.path.chars().any(char::is_control)
            || self.symbol_id.chars().any(char::is_control)
            || (self.path.is_empty()
                && (!self.symbol_id.is_empty()
                    || self.start_line.is_some()
                    || self.end_line.is_some()))
            || self.start_line == Some(0)
            || self.end_line == Some(0)
            || self
                .end_line
                .is_some_and(|end| self.start_line.is_none_or(|start| end < start))
        {
            return Err(CollaborationCodecError::Invalid(
                "invalid source anchor path or line span".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{
        Attribution, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, DiscussionRecordId,
        DiscussionTurnV1, Principal, VisibilityTier,
    };
    #[test]
    fn exact_source_span_round_trips_and_invalid_ranges_fail() {
        let source = CollaborationSourceAnchor {
            revision: CollaborationRevision::GitCommit {
                oid: "a".repeat(40),
            },
            path: "src/main.rs".into(),
            symbol_id: "run".into(),
            start_line: Some(12),
            end_line: Some(18),
            target: None,
        };
        let record = CollaborationOperationEnvelope::new(
            DiscussionRecordId::generate(),
            vec![],
            CollaborationIdempotencyKey::new("source-review").expect("ID"),
            Attribution::human(Principal::new("Reviewer", "")),
            100,
            CollaborationOperationBodyV1::Open {
                blocking: true,
                title: "Review exact lines".into(),
                anchor: CollaborationAnchor::Source {
                    source: source.clone(),
                },
                visibility: VisibilityTier::Public,
                turn: DiscussionTurnV1::new("Explain this change").expect("turn"),
                thread_ref: None,
            },
        )
        .expect("record");
        let decoded = CollaborationOperationEnvelope::decode(&record.encode().expect("canonical"))
            .expect("decode");
        assert_eq!(decoded.operation, record, "all source fields survive sync");
        let mut invalid = source;
        invalid.end_line = Some(11);
        assert!(
            invalid.validate().is_err(),
            "inverted span must be rejected"
        );
        invalid.end_line = Some(18);
        invalid.path.clear();
        assert!(
            invalid.validate().is_err(),
            "line numbers require an exact file path"
        );
    }
}
