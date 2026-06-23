// SPDX-License-Identifier: Apache-2.0
//! Anchored discussions on symbols.
//!
//! A discussion is opened against a symbol (file + symbol name, no line
//! range), accumulates an ordered list of turns, and resolves into one of
//! three terminal states. Anchors travel across renames and cross-file moves
//! — the travel logic lives in `crates/repo/src/discussion_anchor_travel.rs`
//! because it needs source bytes and tree-sitter; this module owns only the
//! shape.
//!
//! Visibility inherits from the repo's annotation-default policy unless
//! explicitly overridden when the discussion is opened.

use serde::{Deserialize, Serialize};

use crate::object::{
    hash::ChangeId, state_attribution::Principal, state_review::SymbolAnchor,
    visibility_tier::VisibilityTier,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionsBlob {
    pub format_version: u8,
    pub discussions: Vec<Discussion>,
}

versioned_msgpack_blob! {
    blob: DiscussionsBlob,
    item: Discussion,
    field: discussions,
    error: DiscussionError,
    codec_err: Encoding,
    version: 1,
}

/// Stable opaque identifier for a discussion. Generated server-side at open
/// time. We use a `String` rather than `ChangeId` to leave room for whatever
/// id scheme the discussion service ends up choosing (likely a UUID).
pub type DiscussionId = String;

/// Pure discussion mutation semantics.
///
/// Adapters generate IDs/timestamps/principals and translate transport errors;
/// this enum is deliberately free of protobuf, tonic, policy, and storage
/// concerns so local state-blob persistence and future hosted storage can
/// share the same operation shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscussionOperation {
    Open {
        id: DiscussionId,
        anchor: SymbolAnchor,
        opened_against_state: ChangeId,
        opened_at: i64,
        thread_ref: Option<String>,
        author: Principal,
        body: String,
        visibility: VisibilityTier,
    },
    AppendTurn {
        discussion_id: DiscussionId,
        author: Principal,
        body: String,
        posted_at: i64,
    },
    Resolve {
        discussion_id: DiscussionId,
        resolution: DiscussionResolution,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discussion {
    pub id: DiscussionId,
    pub anchor: SymbolAnchor,
    pub opened_against_state: ChangeId,
    /// Unix epoch seconds.
    pub opened_at: i64,
    #[serde(default)]
    pub thread_ref: Option<String>,
    pub turns: Vec<DiscussionTurn>,
    pub resolution: DiscussionResolution,
    /// Set by anchor-travel when the symbol body has changed since this
    /// discussion was opened. Reviewers see a marker; resolution still
    /// proceeds normally.
    #[serde(default)]
    pub body_changed_since_open: bool,
    /// Set by anchor-travel when the symbol can't be resolved in the new
    /// state (deleted or unreachable rename). The discussion stays open with
    /// this marker for a human to triage.
    #[serde(default)]
    pub orphaned: bool,
    /// Inherits from namespace policy unless explicitly overridden.
    #[serde(default)]
    pub visibility: VisibilityTier,
    /// Bidirectional link populated when [`DiscussionResolution::ResolvedIntoAnnotation`]
    /// fires. Lets viewers jump from the discussion to the annotation it
    /// produced (and vice versa, via a back-pointer on the annotation).
    #[serde(default)]
    pub resolved_annotation_id: Option<String>,
}

impl DiscussionsBlob {
    pub fn apply_operation(
        &mut self,
        operation: DiscussionOperation,
    ) -> Result<Discussion, DiscussionError> {
        match operation {
            DiscussionOperation::Open {
                id,
                anchor,
                opened_against_state,
                opened_at,
                thread_ref,
                author,
                body,
                visibility,
            } => {
                if let Some(existing) = self
                    .discussions
                    .iter()
                    .find(|discussion| discussion.id == id)
                    .cloned()
                {
                    return Ok(existing);
                }
                let discussion = Discussion {
                    id,
                    anchor,
                    opened_against_state,
                    opened_at,
                    thread_ref,
                    turns: vec![DiscussionTurn {
                        author,
                        body,
                        posted_at: opened_at,
                    }],
                    resolution: DiscussionResolution::Open,
                    body_changed_since_open: false,
                    orphaned: false,
                    visibility,
                    resolved_annotation_id: None,
                };
                discussion.validate()?;
                self.discussions.push(discussion.clone());
                Ok(discussion)
            }
            DiscussionOperation::AppendTurn {
                discussion_id,
                author,
                body,
                posted_at,
            } => {
                let discussion = self
                    .discussions
                    .iter_mut()
                    .find(|discussion| discussion.id == discussion_id)
                    .ok_or_else(|| DiscussionError::DiscussionNotFound(discussion_id.clone()))?;
                discussion.turns.push(DiscussionTurn {
                    author,
                    body,
                    posted_at,
                });
                discussion.validate()?;
                Ok(discussion.clone())
            }
            DiscussionOperation::Resolve {
                discussion_id,
                resolution,
            } => {
                let discussion = self
                    .discussions
                    .iter_mut()
                    .find(|discussion| discussion.id == discussion_id)
                    .ok_or_else(|| DiscussionError::DiscussionNotFound(discussion_id.clone()))?;
                if let DiscussionResolution::ResolvedIntoAnnotation { annotation_id } = &resolution
                {
                    discussion.resolved_annotation_id = Some(annotation_id.clone());
                }
                discussion.resolution = resolution;
                discussion.validate()?;
                Ok(discussion.clone())
            }
        }
    }
}

impl Discussion {
    pub fn validate(&self) -> Result<(), DiscussionError> {
        if self.id.is_empty() {
            return Err(DiscussionError::EmptyId);
        }
        if self.anchor.file.is_empty() {
            return Err(DiscussionError::EmptyAnchorFile);
        }
        if self.anchor.symbol.is_empty() {
            return Err(DiscussionError::EmptyAnchorSymbol);
        }
        for turn in &self.turns {
            turn.validate()?;
        }
        if let DiscussionResolution::Dismissed { reason } = &self.resolution
            && reason.trim().is_empty()
        {
            return Err(DiscussionError::EmptyDismissReason);
        }
        if matches!(
            self.resolution,
            DiscussionResolution::ResolvedIntoAnnotation { .. }
        ) && self.resolved_annotation_id.is_none()
        {
            return Err(DiscussionError::MissingAnnotationLink);
        }
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        matches!(self.resolution, DiscussionResolution::Open)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionTurn {
    pub author: Principal,
    pub body: String,
    /// Unix epoch seconds.
    pub posted_at: i64,
}

impl DiscussionTurn {
    pub fn validate(&self) -> Result<(), DiscussionError> {
        if self.body.trim().is_empty() {
            return Err(DiscussionError::EmptyTurnBody);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscussionResolution {
    #[default]
    Open,
    /// The discussion produced an annotation; the annotation is the durable
    /// artifact going forward. The bidirectional link is on
    /// [`Discussion::resolved_annotation_id`] and on the annotation's
    /// metadata back-pointer.
    ResolvedIntoAnnotation { annotation_id: String },
    /// A subsequent edit addressed the discussion's concern. The state ID
    /// pinpoints which edit was the answer.
    ResolvedByEdit { state_id: ChangeId },
    /// The discussion was dismissed without an annotation or follow-up
    /// edit. A non-empty reason is required so future readers know why.
    Dismissed { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum DiscussionError {
    #[error("unsupported discussions blob version {0}")]
    UnsupportedVersion(u8),
    #[error("discussion id must not be empty")]
    EmptyId,
    #[error("discussion anchor must reference a non-empty file")]
    EmptyAnchorFile,
    #[error("discussion anchor must reference a non-empty symbol")]
    EmptyAnchorSymbol,
    #[error("discussion turn body must not be empty")]
    EmptyTurnBody,
    #[error("dismissed discussion must include a non-empty reason")]
    EmptyDismissReason,
    #[error("resolved-into-annotation discussion must set resolved_annotation_id")]
    MissingAnnotationLink,
    #[error("discussion {0} not found")]
    DiscussionNotFound(String),
    #[error("discussions blob encoding error: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_principal() -> Principal {
        Principal::new("Alice", "alice@example.com")
    }

    fn sample_discussion() -> Discussion {
        Discussion {
            id: "disc-1".into(),
            anchor: SymbolAnchor::new("src/lib.rs", "foo"),
            opened_against_state: ChangeId::from_bytes([7; 16]),
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: sample_principal(),
                body: "why does this branch exist?".into(),
                posted_at: 1_700_000_000,
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            orphaned: false,
            visibility: VisibilityTier::default(),
            resolved_annotation_id: None,
        }
    }

    #[test]
    fn open_discussion_validates() {
        sample_discussion().validate().unwrap();
    }

    #[test]
    fn dismissed_with_empty_reason_rejected() {
        let mut d = sample_discussion();
        d.resolution = DiscussionResolution::Dismissed {
            reason: "  ".into(),
        };
        assert!(matches!(
            d.validate(),
            Err(DiscussionError::EmptyDismissReason)
        ));
    }

    #[test]
    fn resolved_into_annotation_requires_link() {
        let mut d = sample_discussion();
        d.resolution = DiscussionResolution::ResolvedIntoAnnotation {
            annotation_id: "ann-7".into(),
        };
        d.resolved_annotation_id = None;
        assert!(matches!(
            d.validate(),
            Err(DiscussionError::MissingAnnotationLink)
        ));
        d.resolved_annotation_id = Some("ann-7".into());
        d.validate().unwrap();
    }

    #[test]
    fn empty_turn_body_rejected() {
        let mut d = sample_discussion();
        d.turns[0].body = "   ".into();
        assert!(matches!(d.validate(), Err(DiscussionError::EmptyTurnBody)));
    }

    #[test]
    fn blob_roundtrip() {
        let blob = DiscussionsBlob::new(vec![sample_discussion()]);
        let bytes = blob.encode().unwrap();
        let decoded = DiscussionsBlob::decode(&bytes).unwrap();
        assert_eq!(blob, decoded);
    }

    #[test]
    fn body_changed_marker_round_trips() {
        let mut d = sample_discussion();
        d.body_changed_since_open = true;
        let blob = DiscussionsBlob::new(vec![d]);
        let bytes = blob.encode().unwrap();
        let decoded = DiscussionsBlob::decode(&bytes).unwrap();
        assert!(decoded.discussions[0].body_changed_since_open);
    }

    #[test]
    fn operations_open_append_and_resolve_discussion() {
        let state_id = ChangeId::from_bytes([9; 16]);
        let author = sample_principal();
        let mut blob = DiscussionsBlob::new(Vec::new());

        let opened = blob
            .apply_operation(DiscussionOperation::Open {
                id: "disc-op".into(),
                anchor: SymbolAnchor::new("src/lib.rs", "foo"),
                opened_against_state: state_id,
                opened_at: 11,
                thread_ref: Some("main".into()),
                author: author.clone(),
                body: "please explain".into(),
                visibility: VisibilityTier::default(),
            })
            .unwrap();
        assert_eq!(opened.turns.len(), 1);

        let appended = blob
            .apply_operation(DiscussionOperation::AppendTurn {
                discussion_id: "disc-op".into(),
                author,
                body: "here is the answer".into(),
                posted_at: 12,
            })
            .unwrap();
        assert_eq!(appended.turns.len(), 2);

        let resolved = blob
            .apply_operation(DiscussionOperation::Resolve {
                discussion_id: "disc-op".into(),
                resolution: DiscussionResolution::Dismissed {
                    reason: "answered inline".into(),
                },
            })
            .unwrap();
        assert!(matches!(
            resolved.resolution,
            DiscussionResolution::Dismissed { .. }
        ));
    }
}
