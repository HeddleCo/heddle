//! Immutable hosted testimony about original authority at first durable receipt.
//! This is separate from causal acceptance and from current review/landing policy.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CollaborationActor, ContentHash,
    thread_replication::{
        ThreadOperation, ThreadOperationBody, integration::TrustedHostedExecutor,
        metadata::ThreadControl,
    },
};
use crate::error::{HeddleError, Result};

pub const FORMAT: &str = "heddle-thread-authority-admission-v1";
pub const MAX_BYTES: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadAuthorityAdmission {
    pub version: u16,
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub thread: ContentHash,
    pub operation: ContentHash,
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
        if self.version != 1
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
        let ThreadOperationBody::Metadata(bytes) = &operation.body else {
            return Err(invalid(
                "authority admission requires original Thread metadata",
            ));
        };
        let control = ThreadControl::decode(bytes)?;
        if self.operation != operation.id()?
            || self.thread != operation.thread
            || self.publisher != operation.publisher
            || self.actor != control.actor
            || self.spool != control.spool
            || self.authority_digest != control.authority_digest
        {
            return Err(invalid(
                "authority admission differs from original operation",
            ));
        }
        Ok(())
    }
}
fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}
