//! Immutable hosted testimony about original authority at first durable receipt.
//! This is separate from causal acceptance and from current review/landing policy.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CollaborationActor, ContentHash,
    thread_replication::{
        SourceAuthor, ThreadOperation, ThreadOperationBody, integration::TrustedHostedExecutor,
        metadata::ThreadControl,
    },
};
use crate::error::{HeddleError, Result};

pub const FORMAT: &str = "heddle-thread-authority-admission-v2";
pub const MAX_BYTES: usize = 2048;

/// An admission never changes kind when relayed: a claim receipt cannot
/// authorize a source operation with coincidentally equal bytes or identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OriginalAuthoritySubject {
    Operation(ContentHash),
    OwnershipClaim(ContentHash),
}
impl OriginalAuthoritySubject {
    pub fn id(&self) -> ContentHash {
        match self { Self::Operation(id) | Self::OwnershipClaim(id) => *id }
    }
    pub fn operation_id(&self) -> Option<ContentHash> {
        match self { Self::Operation(id) => Some(*id), Self::OwnershipClaim(_) => None }
    }
    pub fn claim_id(&self) -> Option<ContentHash> {
        match self { Self::OwnershipClaim(id) => Some(*id), Self::Operation(_) => None }
    }
}

/// Signed original account identity shared by fresh admission and retained
/// testimony. Local-key authors have no account binding to relabel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginalAuthorityBinding {
    pub spool: Uuid,
    pub actor: CollaborationActor,
    pub authority_digest: ContentHash,
}
impl OriginalAuthorityBinding {
    pub fn from_operation(operation: &ThreadOperation) -> Result<Option<Self>> {
        if let ThreadOperationBody::Metadata(bytes) = &operation.body {
            let control = ThreadControl::decode(bytes)?;
            return Ok(Some(Self { spool: control.spool, actor: control.actor, authority_digest: control.authority_digest }));
        }
        match operation.source_author()? {
            Some(SourceAuthor::Account { spool, actor, authority_digest, authority }) => {
                SourceAuthor::Account { spool, actor: actor.clone(), authority_digest, authority }.validate()?;
                Ok(Some(Self { spool, actor, authority_digest }))
            }
            Some(SourceAuthor::LocalKey) | None => Ok(None),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadAuthorityAdmission {
    pub version: u16,
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub thread: ContentHash,
    pub subject: OriginalAuthoritySubject,
    pub actor: CollaborationActor,
    pub publisher: [u8; 32],
    pub authority_digest: ContentHash,
    pub executor: [u8; 32],
    /// Executor-observed first durable admission, never supplied by the author.
    /// This timestamp does not revive an expired credential at fresh admission.
    pub admitted_at_ms: i64,
}
impl ThreadAuthorityAdmission {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 2
            || self.spool.is_nil()
            || self.actor.principal_id.is_nil()
            || self.publisher == [0; 32]
            || self.executor == [0; 32]
            || self.admitted_at_ms < 0
            || self.actor.agent_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
            })
        {
            return Err(invalid("invalid original-author admission statement"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("authority admission statement exceeds byte bound"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(invalid("authority admission statement exceeds byte bound"));
        }
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical authority admission statement"));
        }
        Ok(value)
    }
    /// Compare testimony with an independently admitted immutable Spool/executor
    /// pin and the original operation. Caller also verifies both signatures.
    pub fn authorize(
        &self,
        operation: &ThreadOperation,
        trust: &TrustedHostedExecutor,
    ) -> Result<()> {
        self.encode()?;
        if self.spool != trust.spool
            || self.spool_genesis != trust.spool_genesis
            || self.executor != trust.executor
        {
            return Err(invalid(
                "authority admission differs from independently pinned executor",
            ));
        }
        let binding = OriginalAuthorityBinding::from_operation(operation)?.ok_or_else(||
            invalid("account admission requires original authored account work"))?;
        if self.subject != OriginalAuthoritySubject::Operation(operation.id()?)
            || self.thread != operation.thread
            || self.publisher != operation.publisher
            || self.actor != binding.actor
            || self.spool != binding.spool
            || self.authority_digest != binding.authority_digest
        {
            return Err(invalid(
                "authority admission differs from original operation",
            ));
        }
        Ok(())
    }
    pub fn authorize_claim(
        &self,
        claim: &super::thread_replication::ownership_claim::ThreadOwnershipClaim,
        genesis: &super::thread_replication::ThreadGenesis,
        trust: &TrustedHostedExecutor,
    ) -> Result<()> {
        self.encode()?;
        claim.validate_genesis(genesis)?;
        let SourceAuthor::Account { spool, actor, authority_digest, .. } = &claim.acceptance else {
            return Err(invalid("claim admission requires signed account acceptance"));
        };
        if self.spool != trust.spool || self.spool_genesis != trust.spool_genesis || self.executor != trust.executor {
            return Err(invalid("authority admission differs from independently pinned executor"));
        }
        if self.subject != OriginalAuthoritySubject::OwnershipClaim(claim.id()?)
            || self.thread != claim.thread || self.publisher != claim.accepting_publisher
            || self.actor != *actor || self.spool != *spool || self.authority_digest != *authority_digest {
            return Err(invalid("authority admission differs from original ownership claim"));
        }
        Ok(())
    }
}
fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}
