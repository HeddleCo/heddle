// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::object::{Attribution, ChangeId, ContentHash, StateId, VisibilityTier};

use super::{
    CollabOpId, CollaborationCodecError, CollaborationIdempotencyKey, DiscussionRecordId,
    LegacyDiscussionId, LegacySourceLocator,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationAnchor {
    Repository,
    State {
        state_id: StateId,
    },
    Change {
        change_id: ChangeId,
    },
    Path {
        state_id: StateId,
        path: String,
    },
    Symbol {
        state_id: StateId,
        path: String,
        symbol: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionTurnV1 {
    pub body: String,
    pub content_hash: ContentHash,
}

impl DiscussionTurnV1 {
    pub fn new(body: impl Into<String>) -> Result<Self, CollaborationCodecError> {
        let body = body.into();
        require_text(&body, "turn body")?;
        let content_hash = ContentHash::compute_typed("collaboration-turn", body.as_bytes());
        Ok(Self { body, content_hash })
    }

    pub(crate) fn validate(&self) -> Result<(), CollaborationCodecError> {
        require_text(&self.body, "turn body")?;
        if ContentHash::compute_typed("collaboration-turn", self.body.as_bytes())
            != self.content_hash
        {
            return Err(CollaborationCodecError::Invalid(
                "turn content hash does not match its body".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationResolution {
    AddressedByState { state_id: StateId },
    AddressedByChange { change_id: ChangeId },
    Dismissed { reason: String },
    Annotation { annotation_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LegacyDiscussionResolutionV1 {
    Open,
    AddressedByState { state_id: StateId },
    Dismissed { reason: String },
    Annotation { annotation_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationOperationBodyV1 {
    Open {
        title: String,
        anchor: CollaborationAnchor,
        visibility: VisibilityTier,
        turn: DiscussionTurnV1,
    },
    AppendTurn {
        turn: DiscussionTurnV1,
    },
    Resolve {
        resolution: CollaborationResolution,
    },
    Reopen {
        reason: String,
    },
    ResolveConflict {
        competing: Vec<CollabOpId>,
        selected: CollabOpId,
    },
    LegacyImported {
        source: LegacySourceLocator,
        legacy_discussion_id: LegacyDiscussionId,
        aliases: Vec<LegacySourceLocator>,
        title: String,
        anchor: CollaborationAnchor,
        visibility: VisibilityTier,
        turns: Vec<DiscussionTurnV1>,
        resolution: LegacyDiscussionResolutionV1,
    },
}

impl CollaborationOperationBodyV1 {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::AppendTurn { .. } => "append_turn",
            Self::Resolve { .. } => "resolve",
            Self::Reopen { .. } => "reopen",
            Self::ResolveConflict { .. } => "resolve_conflict",
            Self::LegacyImported { .. } => "legacy_imported",
        }
    }

    pub(crate) fn validate(&self) -> Result<(), CollaborationCodecError> {
        match self {
            Self::Open {
                title,
                anchor,
                turn,
                ..
            } => {
                require_text(title, "discussion title")?;
                validate_anchor(anchor)?;
                turn.validate()
            }
            Self::AppendTurn { turn } => turn.validate(),
            Self::Resolve { resolution } => validate_resolution(resolution),
            Self::Reopen { reason } => require_text(reason, "reopen reason"),
            Self::ResolveConflict {
                competing,
                selected,
            } => {
                if competing.len() < 2 || !competing.contains(selected) {
                    return Err(CollaborationCodecError::Invalid(
                        "conflict resolution must select one of at least two competing operations"
                            .to_string(),
                    ));
                }
                if competing.windows(2).any(|ids| ids[0] >= ids[1]) {
                    return Err(CollaborationCodecError::Invalid(
                        "competing operation ids must be sorted and unique".to_string(),
                    ));
                }
                Ok(())
            }
            Self::LegacyImported {
                title,
                anchor,
                aliases,
                turns,
                resolution,
                ..
            } => {
                require_text(title, "discussion title")?;
                validate_anchor(anchor)?;
                if aliases.windows(2).any(|values| values[0] >= values[1]) {
                    return Err(CollaborationCodecError::Invalid(
                        "legacy aliases must be sorted and unique".to_string(),
                    ));
                }
                if turns.is_empty() {
                    return Err(CollaborationCodecError::Invalid(
                        "legacy import must contain a turn".to_string(),
                    ));
                }
                for turn in turns {
                    turn.validate()?;
                }
                validate_legacy_resolution(resolution)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationOperationEnvelope {
    pub discussion_id: DiscussionRecordId,
    pub parents: Vec<CollabOpId>,
    pub idempotency_key: CollaborationIdempotencyKey,
    pub author: Attribution,
    pub occurred_at_ms: i64,
    pub body: CollaborationOperationBodyV1,
}

impl CollaborationOperationEnvelope {
    pub fn new(
        discussion_id: DiscussionRecordId,
        mut parents: Vec<CollabOpId>,
        idempotency_key: CollaborationIdempotencyKey,
        author: Attribution,
        occurred_at_ms: i64,
        body: CollaborationOperationBodyV1,
    ) -> Result<Self, CollaborationCodecError> {
        parents.sort();
        parents.dedup();
        let operation = Self {
            discussion_id,
            parents,
            idempotency_key,
            author,
            occurred_at_ms,
            body,
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn encode(&self) -> Result<Vec<u8>, CollaborationCodecError> {
        super::codec::encode(self)
    }

    pub fn decode(
        bytes: &[u8],
    ) -> Result<super::DecodedCollaborationOperation, CollaborationCodecError> {
        super::codec::decode(bytes)
    }

    pub(crate) fn validate(&self) -> Result<(), CollaborationCodecError> {
        if self.parents.windows(2).any(|ids| ids[0] >= ids[1]) {
            return Err(CollaborationCodecError::Invalid(
                "parent operation ids must be sorted and unique".to_string(),
            ));
        }
        if matches!(
            self.body,
            CollaborationOperationBodyV1::Open { .. }
                | CollaborationOperationBodyV1::LegacyImported { .. }
        ) {
            if !self.parents.is_empty() {
                return Err(CollaborationCodecError::Invalid(
                    "discussion root operation cannot have parents".to_string(),
                ));
            }
        } else if self.parents.is_empty() {
            return Err(CollaborationCodecError::Invalid(
                "non-root collaboration operation requires a parent".to_string(),
            ));
        }
        if let CollaborationOperationBodyV1::ResolveConflict { competing, .. } = &self.body
            && competing.iter().any(|id| !self.parents.contains(id))
        {
            return Err(CollaborationCodecError::Invalid(
                "conflict resolution must causally follow every competing operation".to_string(),
            ));
        }
        self.body.validate()
    }
}

fn validate_anchor(anchor: &CollaborationAnchor) -> Result<(), CollaborationCodecError> {
    match anchor {
        CollaborationAnchor::Path { path, .. } => require_text(path, "anchor path"),
        CollaborationAnchor::Symbol { path, symbol, .. } => {
            require_text(path, "anchor path")?;
            require_text(symbol, "anchor symbol")
        }
        CollaborationAnchor::Repository
        | CollaborationAnchor::State { .. }
        | CollaborationAnchor::Change { .. } => Ok(()),
    }
}

fn validate_resolution(value: &CollaborationResolution) -> Result<(), CollaborationCodecError> {
    match value {
        CollaborationResolution::Dismissed { reason } => require_text(reason, "dismiss reason"),
        CollaborationResolution::Annotation { annotation_id } => {
            require_text(annotation_id, "annotation id")
        }
        _ => Ok(()),
    }
}

fn validate_legacy_resolution(
    value: &LegacyDiscussionResolutionV1,
) -> Result<(), CollaborationCodecError> {
    match value {
        LegacyDiscussionResolutionV1::Dismissed { reason } => {
            require_text(reason, "dismiss reason")
        }
        LegacyDiscussionResolutionV1::Annotation { annotation_id } => {
            require_text(annotation_id, "annotation id")
        }
        _ => Ok(()),
    }
}

fn require_text(value: &str, field: &str) -> Result<(), CollaborationCodecError> {
    if value.trim().is_empty() {
        Err(CollaborationCodecError::Invalid(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(())
    }
}
