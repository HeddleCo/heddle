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
    hash::StateId, state_attribution::Principal, state_review::SymbolAnchor,
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

pub fn generate_discussion_id() -> DiscussionId {
    uuid::Uuid::now_v7().to_string()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discussion {
    pub id: DiscussionId,
    pub anchor: SymbolAnchor,
    pub opened_against_state: StateId,
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
    ResolvedByEdit { state_id: StateId },
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
            opened_against_state: StateId::from_bytes([7; 32]),
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
}
