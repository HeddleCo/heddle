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
    pub fn id(&self) -> StateAttachmentId {
        let bytes = rmp_serde::to_vec_named(self).expect("state attachment encoding is infallible");
        StateAttachmentId::from_hash(ContentHash::compute_typed("state-attachment", &bytes))
    }
}

#[cfg(test)]
mod wire_compat_tests {
    //! Cross-version decode guard for the `SemanticIndex` attachment variant
    //! (heddle#1067 / heddle#1078).
    //!
    //! `StateAttachmentBody` is an externally-tagged rmp-named enum, so a
    //! `SemanticIndex(hash)` value serializes as a one-key map keyed on the
    //! variant name. A consumer built before the variant existed (heddle 0.10.2,
    //! e.g. a `weft` that has not yet bumped its `heddle-objects` pin) has no
    //! matching arm and therefore CANNOT decode such an attachment. This test
    //! pins that hazard so the failure mode is a documented regression guard and
    //! not a field surprise: weft must bump to `=0.10.3` before a heddle release
    //! ships #1067.

    use chrono::{DateTime, Utc};
    use serde::Deserialize;

    use super::*;
    use crate::object::{Attribution, Principal};

    /// A faithful mirror of the **0.10.2** `StateAttachmentBody`, i.e. the arm
    /// set BEFORE `SemanticIndex` was introduced. This is exactly the shape an
    /// older consumer's decoder would carry. The payloads exist only to shape
    /// the decoder — they are never read.
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    enum LegacyBody {
        Context(ContentHash),
        RiskSignals(ContentHash),
        ReviewSignatures(ContentHash),
        Discussions(ContentHash),
        StructuredConflicts(ContentHash),
        Signature(StateSignature),
    }

    /// A 0.10.2 consumer's `StateAttachment`, structurally identical to the
    /// current one but with the legacy (SemanticIndex-less) body enum.
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct LegacyAttachment {
        state_id: StateId,
        body: LegacyBody,
        attribution: Attribution,
        created_at: DateTime<Utc>,
        supersedes: Option<StateAttachmentId>,
    }

    fn sample(body: StateAttachmentBody) -> StateAttachment {
        StateAttachment {
            state_id: StateId::from_bytes([7; 32]),
            body,
            attribution: Attribution::human(Principal::new("Test", "test@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        }
    }

    #[test]
    fn semantic_index_attachment_is_undecodable_by_0_10_2_consumer() {
        let attachment = sample(StateAttachmentBody::SemanticIndex(ContentHash::compute(b"root")));
        let bytes = rmp_serde::to_vec_named(&attachment).expect("encode");

        // A current (0.10.3+) consumer round-trips it cleanly.
        let current: StateAttachment = rmp_serde::from_slice(&bytes).expect("current decoder");
        assert_eq!(current, attachment);

        // A 0.10.2 consumer, whose decoder has no `SemanticIndex` arm, MUST
        // reject it — this is precisely why weft has to bump before a heddle
        // release ships the SemanticIndex attachment.
        let legacy: Result<LegacyAttachment, _> = rmp_serde::from_slice(&bytes);
        assert!(
            legacy.is_err(),
            "a 0.10.2 decoder must fail on a SemanticIndex-tagged attachment"
        );
    }

    #[test]
    fn known_variant_still_decodes_on_0_10_2_consumer() {
        // Control: an attachment kind the 0.10.2 consumer DOES know still
        // decodes with the legacy enum, so the failure above is specifically the
        // new variant and not a framing/format mismatch.
        let attachment = sample(StateAttachmentBody::Context(ContentHash::compute(b"ctx")));
        let bytes = rmp_serde::to_vec_named(&attachment).expect("encode");
        let legacy: Result<LegacyAttachment, _> = rmp_serde::from_slice(&bytes);
        assert!(
            legacy.is_ok(),
            "a 0.10.2 decoder must still handle the Context variant it knows"
        );
    }
}
