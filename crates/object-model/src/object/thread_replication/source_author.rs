//! Original source authorship is signed with the capture, independently of its
//! courier. These codecs bind claims; admission verifies the actual authority.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Capture, metadata::AUTHORITY_FORMAT};
use crate::{
    error::{HeddleError, Result},
    object::{CollaborationActor, ContentHash},
};

pub const MAX_SOURCE_AUTHORITY_BYTES: usize = 64 * 1024;
/// Original source-write authority, independent of the courier's transport RPC.
pub const SOURCE_AUTHORIZATION_METHOD: &str = "/heddle.api.v2alpha1.SyncService/PublishContent";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceAuthor {
    /// The operation publisher is the original local key. A receiver must
    /// independently establish its ownership/delegation for this exact Thread.
    LocalKey,
    Account {
        spool: Uuid,
        actor: CollaborationActor,
        authority_digest: ContentHash,
        #[serde(with = "serde_bytes")]
        authority: Vec<u8>,
    },
}
impl SourceAuthor {
    pub fn account(spool: Uuid, actor: CollaborationActor, authority: Vec<u8>) -> Result<Self> {
        let value = Self::Account {
            spool,
            actor,
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &authority),
            authority,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if let Self::Account {
            spool,
            actor,
            authority_digest,
            authority,
        } = self
        {
            if spool.is_nil()
                || actor.principal_id.is_nil()
                || actor.agent_id.as_ref().is_some_and(|id| {
                    id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                })
            {
                return Err(invalid("invalid original source author identity"));
            }
            if authority.is_empty()
                || authority.len() > MAX_SOURCE_AUTHORITY_BYTES
                || ContentHash::compute_typed(AUTHORITY_FORMAT, authority) != *authority_digest
            {
                return Err(invalid("invalid original source authority binding"));
            }
        }
        Ok(())
    }
}

/// Authorship belongs to a human/device capture, not to derived integration
/// results. Hosted integration/import retain their independent executor proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredCapture {
    pub result: Capture,
    pub author: SourceAuthor,
}
impl AuthoredCapture {
    pub fn local(result: Capture) -> Self {
        Self {
            result,
            author: SourceAuthor::LocalKey,
        }
    }
    pub fn account(
        result: Capture,
        spool: Uuid,
        actor: CollaborationActor,
        authority: Vec<u8>,
    ) -> Result<Self> {
        Ok(Self {
            result,
            author: SourceAuthor::account(spool, actor, authority)?,
        })
    }
}
fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> CollaborationActor {
        CollaborationActor {
            principal_id: Uuid::from_u128(1),
            agent_id: Some("capture-agent".into()),
        }
    }
    #[test]
    fn original_source_authority_binds_exact_envelope_and_bounded_identity() {
        let author = SourceAuthor::account(Uuid::from_u128(2), actor(), vec![3; 32])
            .expect("bounded authority");
        let SourceAuthor::Account {
            authority_digest, ..
        } = &author
        else {
            panic!("account author")
        };
        assert_eq!(
            *authority_digest,
            ContentHash::compute_typed(AUTHORITY_FORMAT, &[3; 32])
        );
        let mut changed = author;
        let SourceAuthor::Account { authority, .. } = &mut changed else {
            panic!("account author")
        };
        authority[0] ^= 1;
        assert!(
            changed.validate().is_err(),
            "authority bytes cannot change under original digest"
        );
        assert!(SourceAuthor::account(Uuid::nil(), actor(), vec![1]).is_err());
        assert!(SourceAuthor::account(Uuid::from_u128(2), actor(), vec![]).is_err());
        assert!(
            SourceAuthor::account(
                Uuid::from_u128(2),
                actor(),
                vec![1; MAX_SOURCE_AUTHORITY_BYTES + 1]
            )
            .is_err()
        );
        let mut invalid_agent = actor();
        invalid_agent.agent_id = Some("\n".into());
        assert!(SourceAuthor::account(Uuid::from_u128(2), invalid_agent, vec![1]).is_err());
        SourceAuthor::LocalKey
            .validate()
            .expect("local key needs no account enrollment");
    }
}
