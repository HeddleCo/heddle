//! Portable identity and references carried by the signed collaboration record.
//! Display attribution is retained separately; it is never an account binding.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::CollaborationCodecError;
use crate::object::{ContentHash, StateId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationScope {
    pub spool: Uuid,
    pub thread: Option<ContentHash>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationActor {
    pub principal_id: Uuid,
    pub agent_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationMetadata {
    pub scope: CollaborationScope,
    pub actor: CollaborationActor,
    pub mentions: Vec<CollaborationMention>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationRecordKind {
    Discussion,
    Context,
    Operation,
    Run,
    Policy,
    Analysis,
    Invitation,
    Grant,
    DiscussionTurn,
    Review,
    Notification,
    AttentionItem,
    Member,
    ApprovalGroup,
    Session,
    SignupInvitation,
    TimelineEvent,
    Artifact,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationMention {
    Spool {
        spool: Uuid,
    },
    Thread {
        spool: Uuid,
        thread: ContentHash,
    },
    State {
        spool: Uuid,
        state: StateId,
    },
    GitCommit {
        spool: Uuid,
        oid: String,
    },
    Checkout {
        spool: Uuid,
        device: [u8; 32],
        id: String,
    },
    Record {
        spool: Option<Uuid>,
        record_kind: CollaborationRecordKind,
        id: String,
    },
    Device {
        key: [u8; 32],
    },
}
impl CollaborationMetadata {
    pub(crate) fn validate(&self) -> Result<(), CollaborationCodecError> {
        if self.scope.spool.is_nil()
            || self.actor.principal_id.is_nil()
            || self.mentions.len() > 128
        {
            return Err(invalid(
                "collaboration requires non-nil scope/actor and at most 128 mentions",
            ));
        }
        if self.actor.agent_id.as_ref().is_some_and(|id| {
            id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control)
        }) {
            return Err(invalid("invalid stable collaboration agent identity"));
        }
        for mention in &self.mentions {
            mention.validate()?;
        }
        Ok(())
    }
}
impl CollaborationMention {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        match self {
            CollaborationMention::Spool { spool }
            | CollaborationMention::Thread { spool, .. }
            | CollaborationMention::State { spool, .. } => {
                if spool.is_nil() {
                    return Err(invalid("mention spool cannot be nil"));
                }
            }
            CollaborationMention::Record { spool, id, .. } => {
                if spool.is_some_and(|id| id.is_nil())
                    || id.trim().is_empty()
                    || id.len() > 1024
                    || id.chars().any(char::is_control)
                {
                    return Err(invalid("invalid stable mention identity"));
                }
            }
            CollaborationMention::Checkout { spool, id, .. } => {
                if spool.is_nil()
                    || id.trim().is_empty()
                    || id.len() > 1024
                    || id.chars().any(char::is_control)
                {
                    return Err(invalid("invalid stable mention identity"));
                }
            }
            CollaborationMention::GitCommit { spool, oid } => {
                if spool.is_nil()
                    || !matches!(oid.len(), 40 | 64)
                    || !oid
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(invalid(
                        "Git mention requires exact lower-case commit identity",
                    ));
                }
            }
            CollaborationMention::Device { .. } => {}
        }
        Ok(())
    }
}

fn invalid(message: &str) -> CollaborationCodecError {
    CollaborationCodecError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{
        Attribution, CollabOpId, CollaborationIdempotencyKey, CollaborationOperationBodyV1,
        CollaborationOperationEnvelope, DiscussionRecordId, DiscussionTurnV1, Principal,
    };
    fn operation() -> CollaborationOperationEnvelope {
        CollaborationOperationEnvelope::new(
            DiscussionRecordId::generate(),
            vec![CollabOpId::from_bytes([9; 32])],
            CollaborationIdempotencyKey::new("append-1").expect("operation identity"),
            Attribution::human(Principal::new("display name", "")),
            1,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("See the exact reviewed state").expect("turn"),
            },
        )
        .expect("operation")
        .with_metadata(CollaborationMetadata {
            scope: CollaborationScope {
                spool: Uuid::from_u128(1),
                thread: Some(ContentHash::from_bytes([3; 32])),
            },
            actor: CollaborationActor {
                principal_id: Uuid::from_u128(2),
                agent_id: Some("agent-device-7".into()),
            },
            mentions: vec![CollaborationMention::State {
                spool: Uuid::from_u128(1),
                state: StateId::from_bytes([4; 32]),
            }],
        })
        .expect("portable actor and mention")
    }
    #[test]
    fn signed_record_identity_retains_and_binds_stable_actor_scope_and_mentions() {
        let original = operation();
        let bytes = original.encode().expect("canonical operation");
        let decoded = CollaborationOperationEnvelope::decode(&bytes).expect("portable decode");
        assert_eq!(
            decoded.operation.metadata, original.metadata,
            "actor and references must survive synchronization"
        );
        assert_eq!(decoded.operation, original);
        let mut changed = original.clone();
        changed.metadata.as_mut().expect("metadata").actor.agent_id = Some("another-agent".into());
        assert_ne!(
            decoded.operation_id,
            CollabOpId::for_bytes(&changed.encode().expect("changed actor")),
            "author binding must change when agent changes"
        );
        changed = original.clone();
        changed
            .metadata
            .as_mut()
            .expect("metadata")
            .mentions
            .clear();
        assert_ne!(
            decoded.operation_id,
            CollabOpId::for_bytes(&changed.encode().expect("removed mention")),
            "mentions belong to the author-signed identity"
        );
        changed = original;
        changed.metadata.as_mut().expect("metadata").scope.spool = Uuid::nil();
        assert!(
            changed.encode().is_err(),
            "invalid scope cannot become a canonical operation"
        );
    }
}
