//! Typed converted content, separate from host witness testimony.
//!
//! The API operation is the job's import-specific signature. Its content and
//! frontier commitments select an exact native Capture and causal operation.
//! This module checks native identity/ancestry and transport commitments;
//! crypto checks signatures, capability-verifier checks owner permission, and
//! repo rechecks publication trust while committing. A job never becomes a
//! `TrustedHostedExecutor` or acquires generic capture/metadata/landing powers.
use api::{
    heddle::api::v1alpha2::{
        ImportContentV1, ImportFrontierV1, ImportIdentityV1, SignedDelegatedImportOperationV1,
    },
    import_authority::{self, VerifiedImportDelegation},
};

use super::{GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody, invalid};
use crate::{error::Result, object::ContentHash};

/// Native content bound to a verified import certificate, without any claim of
/// host commit. Original Git attribution remains inside the original State.
#[derive(Clone, Debug, PartialEq)]
pub struct DelegatedImport {
    signed: SignedDelegatedImportOperationV1,
    converted: ThreadOperation,
}

impl DelegatedImport {
    /// Bind exact native converted content, target, frontier and ancestry. The
    /// caller verifies every native original signature before supplying it.
    pub fn bind(
        signed: &SignedDelegatedImportOperationV1,
        delegation: &VerifiedImportDelegation,
        genesis: &ThreadGenesis,
        original_identity: &ImportIdentityV1,
        converted: &ThreadOperation,
        parents: &[ThreadOperation],
    ) -> Result<Self> {
        import_authority::verify_operation(signed, delegation).map_err(invalid)?;
        let body = signed
            .body
            .as_ref()
            .ok_or_else(|| invalid("import operation missing"))?;
        let identity = delegation
            .body()
            .identity
            .as_ref()
            .ok_or_else(|| invalid("import identity missing"))?;
        let account =
            uuid::Uuid::from_slice(&original_identity.owner_account_uuid).map_err(invalid)?;
        let ThreadOperationBody::Capture(capture) = &converted.body else {
            return Err(invalid(
                "delegated import requires converted Capture content",
            ));
        };
        // One native Capture selects the branch tip; Git ancestors are States
        // in its content closure, not additional native operations or grants.
        // The API job signature authorizes their exact content/frontier. A
        // renewal retains those converter bytes rather than re-signing them
        // with the successor job key and changing their native operation IDs.
        if !matches!(capture.author, super::SourceAuthor::LocalKey)
            || genesis.owner != GenesisOwner::Account(account)
            || identity.spool_uuid != original_identity.spool_uuid
            || identity.spool_genesis_digest != original_identity.spool_genesis_digest
            || genesis.spool
                != uuid::Uuid::from_slice(&body.spool_uuid)
                    .map_err(invalid)?
                    .to_string()
            || genesis.id()?.as_bytes().as_slice() != body.genesis_digest
            || converted.thread.as_bytes().as_slice() != body.target_thread_id
        {
            return Err(invalid(
                "import content differs from owner/genesis/job/target binding",
            ));
        }
        let expected = ImportFrontierV1 {
            format_version: 1,
            thread_id: converted.thread.as_bytes().to_vec(),
            operation_ids: converted
                .parents
                .iter()
                .map(|id| id.as_bytes().to_vec())
                .collect(),
        };
        let resulting = ImportFrontierV1 {
            format_version: 1,
            thread_id: converted.thread.as_bytes().to_vec(),
            operation_ids: vec![converted.id()?.as_bytes().to_vec()],
        };
        let content = ImportContentV1 {
            format_version: 1,
            canonical_capture: rmp_serde::to_vec_named(&capture.result)?,
        };
        if import_authority::frontier_digest(&expected).map_err(invalid)?
            != body.expected_frontier_digest
            || import_authority::frontier_digest(&resulting).map_err(invalid)?
                != body.resulting_frontier_digest
            || import_authority::content_digest(&content).map_err(invalid)?
                != body.resulting_content_digest
        {
            return Err(invalid(
                "import content or complete frontier commitment differs",
            ));
        }
        let bound = Self {
            signed: signed.clone(),
            converted: converted.clone(),
        };
        bound.validate_parents(genesis, parents)?;
        Ok(bound)
    }
    /// Recheck only this exact authenticated, carrier-bound operation.
    pub fn validate_parents(
        &self,
        genesis: &ThreadGenesis,
        parents: &[ThreadOperation],
    ) -> Result<()> {
        self.converted
            .validate_parents_inner(genesis, parents, true)
    }
    /// Original API certificate signature and typed operation bytes.
    pub fn signed(&self) -> &SignedDelegatedImportOperationV1 {
        &self.signed
    }
    /// Exact native converted operation; no host commit is implied.
    pub fn converted(&self) -> &ThreadOperation {
        &self.converted
    }
    /// Existing native operation identity, unchanged by delegation or rotation.
    pub fn id(&self) -> Result<ContentHash> {
        self.converted.id()
    }
}
