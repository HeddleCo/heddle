// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::object::{Attribution, ChangeId, ContentHash, StateId, VisibilityTier};

use super::CollaborationCodecError;
use crate::object::collaboration::{
    COLLABORATION_OPERATION_SCHEMA_VERSION, CollabOpId, CollaborationAnchor,
    CollaborationIdempotencyKey, CollaborationOperationBodyV1, CollaborationOperationEnvelope,
    CollaborationResolution, DiscussionRecordId, DiscussionTurnV1, LegacyDiscussionId,
    LegacyDiscussionResolutionV1, LegacySourceLocator,
};

#[derive(Serialize, Deserialize)]
struct WireOperationV1 {
    schema_version: u16,
    discussion_id: DiscussionRecordId,
    parents: Vec<CollabOpId>,
    idempotency_key: CollaborationIdempotencyKey,
    author: Attribution,
    occurred_at_ms: i64,
    body: WireBodyV1,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum WireAnchorV1 {
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

#[derive(Serialize, Deserialize)]
struct WireTurnV1 {
    body: String,
    content_hash: ContentHash,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum WireResolutionV1 {
    AddressedByState { state_id: StateId },
    AddressedByChange { change_id: ChangeId },
    Dismissed { reason: String },
    Annotation { annotation_id: String },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum WireLegacyResolutionV1 {
    Open,
    AddressedByState { state_id: StateId },
    Dismissed { reason: String },
    Annotation { annotation_id: String },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum WireBodyV1 {
    Open {
        title: String,
        anchor: WireAnchorV1,
        visibility: VisibilityTier,
        turn: WireTurnV1,
    },
    AppendTurn {
        turn: WireTurnV1,
    },
    Resolve {
        resolution: WireResolutionV1,
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
        anchor: WireAnchorV1,
        visibility: VisibilityTier,
        turns: Vec<WireTurnV1>,
        resolution: WireLegacyResolutionV1,
    },
}

pub(super) fn encode(
    operation: &CollaborationOperationEnvelope,
) -> Result<Vec<u8>, CollaborationCodecError> {
    let wire = WireOperationV1 {
        schema_version: COLLABORATION_OPERATION_SCHEMA_VERSION,
        discussion_id: operation.discussion_id,
        parents: operation.parents.clone(),
        idempotency_key: operation.idempotency_key.clone(),
        author: operation.author.clone(),
        occurred_at_ms: operation.occurred_at_ms,
        body: operation.body.clone().into(),
    };
    rmp_serde::to_vec_named(&wire)
        .map_err(|error| CollaborationCodecError::Encoding(error.to_string()))
}

pub(super) fn decode(
    bytes: &[u8],
) -> Result<CollaborationOperationEnvelope, CollaborationCodecError> {
    let wire: WireOperationV1 = rmp_serde::from_slice(bytes)
        .map_err(|error| CollaborationCodecError::Encoding(error.to_string()))?;
    Ok(CollaborationOperationEnvelope {
        discussion_id: wire.discussion_id,
        parents: wire.parents,
        idempotency_key: wire.idempotency_key,
        author: wire.author,
        occurred_at_ms: wire.occurred_at_ms,
        body: wire.body.into(),
    })
}

impl From<CollaborationAnchor> for WireAnchorV1 {
    fn from(value: CollaborationAnchor) -> Self {
        match value {
            CollaborationAnchor::Repository => Self::Repository,
            CollaborationAnchor::State { state_id } => Self::State { state_id },
            CollaborationAnchor::Change { change_id } => Self::Change { change_id },
            CollaborationAnchor::Path { state_id, path } => Self::Path { state_id, path },
            CollaborationAnchor::Symbol {
                state_id,
                path,
                symbol,
            } => Self::Symbol {
                state_id,
                path,
                symbol,
            },
        }
    }
}

impl From<WireAnchorV1> for CollaborationAnchor {
    fn from(value: WireAnchorV1) -> Self {
        match value {
            WireAnchorV1::Repository => Self::Repository,
            WireAnchorV1::State { state_id } => Self::State { state_id },
            WireAnchorV1::Change { change_id } => Self::Change { change_id },
            WireAnchorV1::Path { state_id, path } => Self::Path { state_id, path },
            WireAnchorV1::Symbol {
                state_id,
                path,
                symbol,
            } => Self::Symbol {
                state_id,
                path,
                symbol,
            },
        }
    }
}

impl From<DiscussionTurnV1> for WireTurnV1 {
    fn from(value: DiscussionTurnV1) -> Self {
        Self {
            body: value.body,
            content_hash: value.content_hash,
        }
    }
}

impl From<WireTurnV1> for DiscussionTurnV1 {
    fn from(value: WireTurnV1) -> Self {
        Self {
            body: value.body,
            content_hash: value.content_hash,
        }
    }
}

impl From<CollaborationResolution> for WireResolutionV1 {
    fn from(value: CollaborationResolution) -> Self {
        match value {
            CollaborationResolution::AddressedByState { state_id } => {
                Self::AddressedByState { state_id }
            }
            CollaborationResolution::AddressedByChange { change_id } => {
                Self::AddressedByChange { change_id }
            }
            CollaborationResolution::Dismissed { reason } => Self::Dismissed { reason },
            CollaborationResolution::Annotation { annotation_id } => {
                Self::Annotation { annotation_id }
            }
        }
    }
}

impl From<WireResolutionV1> for CollaborationResolution {
    fn from(value: WireResolutionV1) -> Self {
        match value {
            WireResolutionV1::AddressedByState { state_id } => Self::AddressedByState { state_id },
            WireResolutionV1::AddressedByChange { change_id } => {
                Self::AddressedByChange { change_id }
            }
            WireResolutionV1::Dismissed { reason } => Self::Dismissed { reason },
            WireResolutionV1::Annotation { annotation_id } => Self::Annotation { annotation_id },
        }
    }
}

impl From<LegacyDiscussionResolutionV1> for WireLegacyResolutionV1 {
    fn from(value: LegacyDiscussionResolutionV1) -> Self {
        match value {
            LegacyDiscussionResolutionV1::Open => Self::Open,
            LegacyDiscussionResolutionV1::AddressedByState { state_id } => {
                Self::AddressedByState { state_id }
            }
            LegacyDiscussionResolutionV1::Dismissed { reason } => Self::Dismissed { reason },
            LegacyDiscussionResolutionV1::Annotation { annotation_id } => {
                Self::Annotation { annotation_id }
            }
        }
    }
}

impl From<WireLegacyResolutionV1> for LegacyDiscussionResolutionV1 {
    fn from(value: WireLegacyResolutionV1) -> Self {
        match value {
            WireLegacyResolutionV1::Open => Self::Open,
            WireLegacyResolutionV1::AddressedByState { state_id } => {
                Self::AddressedByState { state_id }
            }
            WireLegacyResolutionV1::Dismissed { reason } => Self::Dismissed { reason },
            WireLegacyResolutionV1::Annotation { annotation_id } => {
                Self::Annotation { annotation_id }
            }
        }
    }
}

impl From<CollaborationOperationBodyV1> for WireBodyV1 {
    fn from(value: CollaborationOperationBodyV1) -> Self {
        match value {
            CollaborationOperationBodyV1::Open {
                title,
                anchor,
                visibility,
                turn,
            } => Self::Open {
                title,
                anchor: anchor.into(),
                visibility,
                turn: turn.into(),
            },
            CollaborationOperationBodyV1::AppendTurn { turn } => {
                Self::AppendTurn { turn: turn.into() }
            }
            CollaborationOperationBodyV1::Resolve { resolution } => Self::Resolve {
                resolution: resolution.into(),
            },
            CollaborationOperationBodyV1::Reopen { reason } => Self::Reopen { reason },
            CollaborationOperationBodyV1::ResolveConflict {
                competing,
                selected,
            } => Self::ResolveConflict {
                competing,
                selected,
            },
            CollaborationOperationBodyV1::LegacyImported {
                source,
                legacy_discussion_id,
                aliases,
                title,
                anchor,
                visibility,
                turns,
                resolution,
            } => Self::LegacyImported {
                source,
                legacy_discussion_id,
                aliases,
                title,
                anchor: anchor.into(),
                visibility,
                turns: turns.into_iter().map(Into::into).collect(),
                resolution: resolution.into(),
            },
        }
    }
}

impl From<WireBodyV1> for CollaborationOperationBodyV1 {
    fn from(value: WireBodyV1) -> Self {
        match value {
            WireBodyV1::Open {
                title,
                anchor,
                visibility,
                turn,
            } => Self::Open {
                title,
                anchor: anchor.into(),
                visibility,
                turn: turn.into(),
            },
            WireBodyV1::AppendTurn { turn } => Self::AppendTurn { turn: turn.into() },
            WireBodyV1::Resolve { resolution } => Self::Resolve {
                resolution: resolution.into(),
            },
            WireBodyV1::Reopen { reason } => Self::Reopen { reason },
            WireBodyV1::ResolveConflict {
                competing,
                selected,
            } => Self::ResolveConflict {
                competing,
                selected,
            },
            WireBodyV1::LegacyImported {
                source,
                legacy_discussion_id,
                aliases,
                title,
                anchor,
                visibility,
                turns,
                resolution,
            } => Self::LegacyImported {
                source,
                legacy_discussion_id,
                aliases,
                title,
                anchor: anchor.into(),
                visibility,
                turns: turns.into_iter().map(Into::into).collect(),
                resolution: resolution.into(),
            },
        }
    }
}
