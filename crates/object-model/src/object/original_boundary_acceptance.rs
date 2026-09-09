//! Explicit present-tense acceptance of immutable originals. This never claims
//! that an expired or revoked original credential authorized a past action.
#[path = "original_boundary_preflight.rs"]
mod preflight;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    ContentHash, StateId,
    thread_authority_admission::OriginalAuthorityBinding,
    thread_replication::{SourceAuthor, ThreadOperation},
};
use crate::error::{HeddleError, Result};

pub const FORMAT: &str = "heddle-original-boundary-acceptance-v1";
pub const MANIFEST_FORMAT: &str = "heddle-original-publication-manifest-v1";
pub const INTENT_FORMAT: &str = "heddle-original-publication-intent-v1";
pub const MAX_RECORDS: usize = 10_384;
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ACCEPTANCE_BYTES: usize = 96 * 1024;

/// The manifest classifies all original records, including ineligible metadata
/// and unclaimed local sources; their inclusion grants no acceptance authority.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ManifestSubject {
    Genesis(ContentHash),
    Source(ContentHash),
    OtherOperation(ContentHash),
    OwnershipClaim(ContentHash),
}
impl ManifestSubject {
    pub fn id(&self) -> ContentHash {
        match self {
            Self::Genesis(id)
            | Self::Source(id)
            | Self::OtherOperation(id)
            | Self::OwnershipClaim(id) => *id,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalManifestEntry {
    pub subject: ManifestSubject,
    pub thread: ContentHash,
    pub publisher: [u8; 32],
    pub authority: Option<OriginalAuthorityBinding>,
}
impl OriginalManifestEntry {
    /// Caller verifies the original signature before building this descriptor.
    pub fn from_operation(operation: &ThreadOperation) -> Result<Self> {
        let id = operation.id()?;
        Ok(Self {
            subject: if operation.source_result()?.is_some() {
                ManifestSubject::Source(id)
            } else {
                ManifestSubject::OtherOperation(id)
            },
            thread: operation.thread,
            publisher: operation.publisher,
            authority: OriginalAuthorityBinding::from_operation(operation)?,
        })
    }
    fn validate(&self) -> Result<()> {
        if self.publisher == [0; 32]
            || self.subject.id().as_bytes() == &[0; 32]
            || self.thread.as_bytes() == &[0; 32]
        {
            return Err(invalid("invalid original manifest identity"));
        }
        if let Some(binding) = &self.authority {
            if binding.spool.is_nil()
                || binding.actor.principal_id.is_nil()
                || binding.actor.agent_id.as_ref().is_some_and(|id| {
                    id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                })
            {
                return Err(invalid("invalid original manifest authority"));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalPublicationManifest {
    pub version: u16,
    pub entries: Vec<OriginalManifestEntry>,
}
impl OriginalPublicationManifest {
    pub fn new(mut entries: Vec<OriginalManifestEntry>) -> Result<Self> {
        if entries.len() > MAX_RECORDS {
            return Err(invalid("original manifest record bound exceeded"));
        }
        entries.sort_by(|a, b| a.subject.cmp(&b.subject));
        let value = Self {
            version: 1,
            entries,
        };
        value.encode()?;
        Ok(value)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1 || self.entries.is_empty() || self.entries.len() > MAX_RECORDS {
            return Err(invalid("original manifest record bound exceeded"));
        }
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            entry.validate()?;
            // Source and OtherOperation share the original operation ID domain.
            let domain = match entry.subject {
                ManifestSubject::Genesis(_) => 0,
                ManifestSubject::Source(_) | ManifestSubject::OtherOperation(_) => 1,
                ManifestSubject::OwnershipClaim(_) => 2,
            };
            if !ids.insert((domain, entry.subject.id())) {
                return Err(invalid("duplicate original manifest identity"));
            }
        }
        if self
            .entries
            .windows(2)
            .any(|pair| pair[0].subject >= pair[1].subject)
        {
            return Err(invalid("original manifest is not sorted"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("original manifest byte bound exceeded"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
            return Err(invalid("original manifest byte bound exceeded"));
        }
        let value: Self =
            preflight::decode(bytes, true, |bytes| Ok(rmp_serde::from_slice(bytes)?))?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical original manifest"));
        }
        Ok(value)
    }
    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(MANIFEST_FORMAT, &self.encode()?))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum BoundaryOriginalKind {
    Source,
    AccountGenesis,
    OwnershipClaim,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationIntent {
    pub spool: Uuid,
    pub spool_genesis: ContentHash,
    pub thread: ContentHash,
    pub revision: StateId,
    /// Digest of the complete ordered pack/index inventory, verified by intake.
    pub inventory: ContentHash,
    pub sharing_policy: Option<ContentHash>,
    pub source: [u8; 32],
    pub destination: [u8; 32],
    pub client_operation_id: Uuid,
}
impl PublicationIntent {
    pub fn id(&self) -> Result<ContentHash> {
        if self.spool.is_nil()
            || self.client_operation_id.is_nil()
            || self.source == [0; 32]
            || self.destination == [0; 32]
        {
            return Err(invalid("invalid boundary publication intent"));
        }
        Ok(ContentHash::compute_typed(
            INTENT_FORMAT,
            &rmp_serde::to_vec_named(self)?,
        ))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalBoundaryAcceptance {
    pub version: u16,
    pub publication_intent: ContentHash,
    pub originals_manifest: ContentHash,
    pub original_account: Uuid,
    pub kinds: BTreeSet<BoundaryOriginalKind>,
    pub accepting_publisher: [u8; 32],
    /// Current explicit acceptance, independent of each immutable original author.
    pub accepting_author: SourceAuthor,
}
impl OriginalBoundaryAcceptance {
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.accepting_author.validate()?;
        let SourceAuthor::Account { actor, .. } = &self.accepting_author else {
            return Err(invalid(
                "boundary acceptance requires explicit account authority",
            ));
        };
        if self.version != 1
            || self.kinds.is_empty()
            || self.original_account.is_nil()
            || actor.principal_id != self.original_account
            || self.accepting_publisher == [0; 32]
        {
            return Err(invalid("invalid boundary acceptance identity"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        if bytes.len() > MAX_ACCEPTANCE_BYTES {
            return Err(invalid("boundary acceptance byte bound exceeded"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_ACCEPTANCE_BYTES {
            return Err(invalid("boundary acceptance byte bound exceeded"));
        }
        let value: Self =
            preflight::decode(bytes, false, |bytes| Ok(rmp_serde::from_slice(bytes)?))?;
        if value.encode()? != bytes {
            return Err(invalid("noncanonical boundary acceptance"));
        }
        Ok(value)
    }
    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(FORMAT, &self.encode()?))
    }
    /// Only verifies exact signed intent/selection. This is NOT current authority
    /// or proof that the original credentials ever authorized an action.
    pub fn selected<'a>(
        &self,
        intent: &PublicationIntent,
        manifest: &'a OriginalPublicationManifest,
    ) -> Result<Vec<&'a OriginalManifestEntry>> {
        self.encode()?;
        let SourceAuthor::Account { spool, .. } = &self.accepting_author else {
            return Err(invalid("account acceptance required"));
        };
        if *spool != intent.spool
            || self.publication_intent != intent.id()?
            || self.originals_manifest != manifest.id()?
        {
            return Err(invalid(
                "boundary acceptance differs from exact publication",
            ));
        }
        let selected: Vec<_> = manifest
            .entries
            .iter()
            .filter(|entry| {
                let Some(authority) = &entry.authority else {
                    return false;
                };
                if authority.spool != intent.spool
                    || authority.actor.principal_id != self.original_account
                {
                    return false;
                }
                let kind = match entry.subject {
                    ManifestSubject::Genesis(_) => BoundaryOriginalKind::AccountGenesis,
                    ManifestSubject::Source(_) => BoundaryOriginalKind::Source,
                    ManifestSubject::OwnershipClaim(_) => BoundaryOriginalKind::OwnershipClaim,
                    ManifestSubject::OtherOperation(_) => return false,
                };
                self.kinds.contains(&kind)
            })
            .collect();
        if selected.is_empty() {
            return Err(invalid("boundary acceptance selects no original"));
        }
        Ok(selected)
    }
}
/// Receipt integration is a subsequent explicit format change. The old receipt
/// remains original-author testimony until its consumers support this enum.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AdmissionBasis {
    OriginalAuthority,
    BoundaryAcceptance { acceptance: ContentHash },
}
fn invalid(message: &str) -> HeddleError {
    HeddleError::InvalidObject(message.into())
}

#[cfg(test)]
#[path = "original_boundary_acceptance_tests.rs"]
mod tests;
