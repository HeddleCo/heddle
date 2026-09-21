// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Attribution, ContentHash, StateId, StateSignature};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateAttachmentId(ContentHash);

impl StateAttachmentId {
    pub fn from_hash(hash: ContentHash) -> Self {
        Self(hash)
    }

    pub fn as_hash(&self) -> &ContentHash {
        &self.0
    }
}

impl std::fmt::Display for StateAttachmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ha-{}", self.0.short())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StateAttachmentBody {
    Context(ContentHash),
    RiskSignals(ContentHash),
    ReviewSignatures(ContentHash),
    Discussions(ContentHash),
    StructuredConflicts(ContentHash),
    /// Content hash of the state's `SemanticIndexRoot` blob (heddle#1067).
    SemanticIndex(ContentHash),
    Signature(StateSignature),
}

/// The kind of a [`StateAttachmentBody`], with the payload projected away.
///
/// Kind is a pure function of the record: [`StateAttachmentBody::kind`] maps a
/// body to its kind with no I/O and no ambiguity. This is the primitive that
/// currency (last-attachment-of-a-kind) and supersession (same-kind guard) are
/// expressed in terms of, and that the wire layer threads through
/// `wire::ObjectId` (heddle#1080, Fable §B(1)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StateAttachmentKind {
    Context,
    RiskSignals,
    ReviewSignatures,
    Discussions,
    StructuredConflicts,
    SemanticIndex,
    Signature,
}

impl StateAttachmentBody {
    /// The [`StateAttachmentKind`] of this body — a pure projection that
    /// discards the payload. Exhaustive by construction: adding a body variant
    /// forces a matching kind arm here.
    pub fn kind(&self) -> StateAttachmentKind {
        match self {
            StateAttachmentBody::Context(_) => StateAttachmentKind::Context,
            StateAttachmentBody::RiskSignals(_) => StateAttachmentKind::RiskSignals,
            StateAttachmentBody::ReviewSignatures(_) => StateAttachmentKind::ReviewSignatures,
            StateAttachmentBody::Discussions(_) => StateAttachmentKind::Discussions,
            StateAttachmentBody::StructuredConflicts(_) => StateAttachmentKind::StructuredConflicts,
            StateAttachmentBody::SemanticIndex(_) => StateAttachmentKind::SemanticIndex,
            StateAttachmentBody::Signature(_) => StateAttachmentKind::Signature,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateAttachment {
    pub state_id: StateId,
    pub body: StateAttachmentBody,
    pub attribution: Attribution,
    pub created_at: DateTime<Utc>,
    pub supersedes: Option<StateAttachmentId>,
}

impl StateAttachment {
    /// Encode the canonical named-field msgpack representation shared by
    /// packs and object transfer.
    pub fn encode_current_msgpack(&self) -> crate::error::Result<Vec<u8>> {
        Ok(rmp_serde::to_vec_named(self)?)
    }

    /// Decode the canonical named-field msgpack representation.
    pub fn decode_current_msgpack(bytes: &[u8]) -> crate::error::Result<Self> {
        Ok(rmp_serde::from_slice(bytes)?)
    }

    pub fn id(&self) -> StateAttachmentId {
        let bytes = rmp_serde::to_vec_named(self).expect("state attachment encoding is infallible");
        StateAttachmentId::from_hash(ContentHash::compute_typed("state-attachment", &bytes))
    }
}

