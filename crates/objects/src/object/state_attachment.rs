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
    Signature(StateSignature),
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
    pub fn id(&self) -> StateAttachmentId {
        let bytes = rmp_serde::to_vec_named(self).expect("state attachment encoding is infallible");
        StateAttachmentId::from_hash(ContentHash::compute_typed("state-attachment", &bytes))
    }
}
