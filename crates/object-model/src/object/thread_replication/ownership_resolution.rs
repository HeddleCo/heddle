//! Owner adjudication of conflicting explicit ownership claims over one Thread.
//! The surviving claim and the complete accepted source frontier are signed by
//! the adjudicating owner authority. Signature and account capability admission
//! are separate verification layers, exactly as for the ownership claim.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{GenesisOwner, SourceAuthor, ThreadGenesis, bounded, invalid};
use crate::{error::Result, object::ContentHash};

pub const FORMAT: &str = "heddle-thread-ownership-resolution-v1";
pub const METHOD: &str = "/heddle.api.v1alpha2.ThreadService/ResolveOwnershipConflict";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadOwnershipResolution {
    pub version: u16,
    pub spool: Uuid,
    /// Thread identity being adjudicated.
    pub thread: ContentHash,
    /// Claim id of the surviving claim; must be a member of `conflicting_claims`.
    pub winning_claim: ContentHash,
    /// The full stored conflicting claim-id set, including the winner (>= 2).
    pub conflicting_claims: BTreeSet<ContentHash>,
    /// Complete accepted source frontier observed at resolution time.
    pub frontier: BTreeSet<ContentHash>,
    /// Immutable original local owner who chooses the surviving claim.
    pub local_owner: [u8; 32],
    /// Fresh recipient publisher accepting the chosen account ownership.
    pub accepting_publisher: [u8; 32],
    pub acceptance: SourceAuthor,
    pub occurred_at_ms: i64,
}
impl ThreadOwnershipResolution {
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.acceptance.validate()?;
        if self.version != 1
            || self.local_owner == [0; 32]
            || self.accepting_publisher == [0; 32]
            || self.spool.is_nil()
            || self.occurred_at_ms <= 0
            || self.conflicting_claims.len() < 2
            || self.conflicting_claims.len() > 128
            || self.frontier.len() > 128
            || !self.conflicting_claims.contains(&self.winning_claim)
            || !matches!(&self.acceptance, SourceAuthor::Account { spool, .. } if *spool == self.spool)
        {
            return Err(invalid("invalid Thread ownership resolution"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical Thread ownership resolution"));
        }
        Ok(value)
    }
    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(FORMAT, &self.encode()?))
    }
    pub fn validate_genesis(&self, genesis: &ThreadGenesis) -> Result<()> {
        self.encode()?;
        if self.thread != genesis.id()?
            || genesis.spool != self.spool.to_string()
            || genesis.owner != GenesisOwner::LocalKey(self.local_owner)
        {
            return Err(invalid("resolution differs from immutable local ownership"));
        }
        Ok(())
    }
    pub fn account(&self) -> Result<uuid::Uuid> {
        let SourceAuthor::Account { actor, .. } = &self.acceptance else {
            return Err(invalid("resolution requires account acceptance"));
        };
        Ok(actor.principal_id)
    }
}
