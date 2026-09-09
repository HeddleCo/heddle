//! Hosted testimony about an original account-owned Thread genesis. This proves
//! first admission; it grants neither current disclosure nor account enrollment.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    ContentHash,
    thread_replication::{GenesisOwner, ThreadGenesis, integration::TrustedHostedExecutor},
};
use crate::error::{HeddleError, Result};
pub const FORMAT: &str = "heddle-thread-genesis-admission-v1";
pub const ENVELOPE_FORMAT: &str = "heddle-thread-genesis-authority-v1";
pub const MAX_BYTES: usize = 2048;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGenesisAdmission {
    pub version: u16,
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub thread: ContentHash,
    pub owner: Uuid,
    pub creator: [u8; 32],
    pub authority_digest: ContentHash,
    pub executor: [u8; 32],
    pub admitted_at_ms: i64,
}
impl ThreadGenesisAdmission {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
            || self.spool.is_nil()
            || self.owner.is_nil()
            || self.creator == [0; 32]
            || self.executor == [0; 32]
            || self.admitted_at_ms < 0
        {
            return Err(invalid("invalid original genesis admission"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("genesis admission exceeds bound"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(invalid("genesis admission exceeds bound"));
        }
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical genesis admission"));
        }
        Ok(value)
    }
    pub fn authorize(
        &self,
        genesis: &ThreadGenesis,
        envelope: &[u8],
        trust: &TrustedHostedExecutor,
    ) -> Result<()> {
        self.encode()?;
        if self.spool != trust.spool
            || self.spool_genesis != trust.spool_genesis
            || self.executor != trust.executor
        {
            return Err(invalid(
                "genesis admission differs from independently pinned executor",
            ));
        }
        if envelope.is_empty()
            || envelope.len() > 64 * 1024
            || self.thread != genesis.id()?
            || self.spool.to_string() != genesis.spool
            || genesis.owner != GenesisOwner::Account(self.owner)
            || self.creator != genesis.creator
            || self.authority_digest != ContentHash::compute_typed(ENVELOPE_FORMAT, envelope)
        {
            return Err(invalid(
                "genesis admission differs from original creator proof",
            ));
        }
        Ok(())
    }
}
fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}
