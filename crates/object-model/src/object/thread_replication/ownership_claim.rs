//! Explicit transition from immutable local-key genesis ownership to an account.
//! Signatures and account capability admission are separate verification layers.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{GenesisOwner, SourceAuthor, ThreadGenesis, bounded, invalid};
use crate::{error::Result, object::ContentHash};

pub const FORMAT: &str = "heddle-thread-ownership-claim-v1";
pub const METHOD: &str = "/heddle.api.v2alpha1.ThreadService/ClaimThreadOwnership";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadOwnershipClaim {
    pub version: u16,
    pub thread: ContentHash,
    pub prior_local_key: [u8; 32],
    pub accepting_publisher: [u8; 32],
    pub acceptance: SourceAuthor,
    /// Complete observed source frontier, signed as the former-key authority cutoff.
    pub source_frontier: BTreeSet<ContentHash>,
}
impl ThreadOwnershipClaim {
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.acceptance.validate()?;
        if self.version != 1
            || self.prior_local_key == [0; 32]
            || self.accepting_publisher == [0; 32]
            || self.source_frontier.len() > 128
            || !matches!(self.acceptance, SourceAuthor::Account { .. })
        {
            return Err(invalid("invalid explicit Thread ownership claim"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical Thread ownership claim"));
        }
        Ok(value)
    }
    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(FORMAT, &self.encode()?))
    }
    pub fn validate_genesis(&self, genesis: &ThreadGenesis) -> Result<()> {
        self.encode()?;
        let SourceAuthor::Account { spool, .. } = &self.acceptance else {
            return Err(invalid("Thread claim requires account acceptance"));
        };
        if self.thread != genesis.id()?
            || genesis.spool != spool.to_string()
            || genesis.owner != GenesisOwner::LocalKey(self.prior_local_key)
        {
            return Err(invalid(
                "Thread claim differs from immutable local ownership",
            ));
        }
        Ok(())
    }
    pub fn account(&self) -> Result<uuid::Uuid> {
        let SourceAuthor::Account { actor, .. } = &self.acceptance else {
            return Err(invalid("Thread claim requires account acceptance"));
        };
        Ok(actor.principal_id)
    }
}
