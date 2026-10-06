//! HYBRID signature and exact statement verification.
//!
//! API owns canonical domains, witness sets and retirement proofs. Native
//! originals retain their existing formats and signatures. Capability-verifier
//! supplies the portable owner-authorized import scope. Part 2 must re-resolve
//! these staged values through repo's mutation-time trust transaction.
use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec::Reject,
    import_authority::{self as contract, WitnessPayload},
    witness_trust::{self, ResolvedWitnessStatement, VerifiedWitnessSet},
};
use heddle_object_model::object::thread_replication::{
    ThreadGenesis, ThreadOperation, delegated_import::DelegatedImport,
};
use heddleco_capability_verifier::import_delegation::VerifiedImportDelegation;

use crate::{
    SignerError,
    thread_operation::{SignedGenesis, SignedOperation},
};

#[cfg(test)]
#[path = "import_authority_tests.rs"]
mod tests;

/// Typed contract, native signature and account-authority failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Contract(#[from] Reject),
    #[error(transparent)]
    Native(#[from] crate::thread_operation::Error),
    #[error(transparent)]
    Object(#[from] heddle_object_model::error::HeddleError),
    #[error(transparent)]
    Signature(#[from] SignerError),
    #[error(transparent)]
    Authority(#[from] heddleco_capability_verifier::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Exact original signed statement/proof, authenticated at one fresh set.
/// This is evidence for historical authority; it is not durable permission.
#[derive(Clone, Debug)]
pub struct WitnessEvidence {
    signed: host::SignedHostedWitnessStatementV1,
    proof: Option<host::HostedWitnessHistoryProofV1>,
    resolved: ResolvedWitnessStatement,
}
impl WitnessEvidence {
    pub fn resolve(
        set: &VerifiedWitnessSet,
        signed: &host::SignedHostedWitnessStatementV1,
        proof: Option<&host::HostedWitnessHistoryProofV1>,
        new_work: bool,
        now_ms: i64,
    ) -> Result<Self> {
        let resolved = witness_trust::resolve_statement(set, signed, proof, new_work, now_ms)?;
        Ok(Self {
            signed: signed.clone(),
            proof: proof.cloned(),
            resolved,
        })
    }
    pub fn signed(&self) -> &host::SignedHostedWitnessStatementV1 {
        &self.signed
    }
    pub fn proof(&self) -> Option<&host::HostedWitnessHistoryProofV1> {
        self.proof.as_ref()
    }
    pub fn resolved(&self) -> &ResolvedWitnessStatement {
        &self.resolved
    }
    pub fn recheck(&self, set: &VerifiedWitnessSet, now_ms: i64) -> Result<()> {
        witness_trust::recheck_context(&self.resolved, set, &self.signed, now_ms)?;
        Ok(())
    }
}

/// Retain original native signatures, not merely a carried signer selector.
pub fn verify_native_genesis(
    record: &wire::SignedRecord,
) -> Result<(SignedGenesis, ThreadGenesis)> {
    if record.format != heddle_object_model::object::thread_replication::GENESIS_FORMAT {
        return Err(Reject::Version.into());
    }
    let genesis = ThreadGenesis::decode(&record.canonical_record)?;
    if record.signatures.len() != 1 {
        return Err(Reject::Signature.into());
    }
    let s = &record.signatures[0];
    if s.public_key != genesis.creator {
        return Err(Reject::Signature.into());
    }
    let original = SignedGenesis {
        canonical: record.canonical_record.clone(),
        signature: s.signature.clone(),
    };
    let genesis = original.verify()?;
    Ok((original, genesis))
}
pub fn verify_native_operation(
    record: &wire::SignedRecord,
) -> Result<(SignedOperation, ThreadOperation)> {
    if record.format != heddle_object_model::object::thread_replication::OPERATION_FORMAT {
        return Err(Reject::Version.into());
    }
    let operation = ThreadOperation::decode(&record.canonical_record)?;
    if record.signatures.len() != 1 || record.signatures[0].public_key != operation.publisher {
        return Err(Reject::Signature.into());
    }
    let signed = SignedOperation {
        canonical: record.canonical_record.clone(),
        signature: record.signatures[0].signature.clone(),
    };
    let operation = signed.verify()?;
    Ok((signed, operation))
}

/// Job signature + owner-authorized scope + exact native content/ancestry. A
/// publication witness is independently required before durable installation.
/// The sole verified delegation also binds the native converter job key.
pub fn verify_delegated_import(
    signed: &wire::SignedDelegatedImportOperationV1,
    delegation: &VerifiedImportDelegation,
    genesis: &VerifiedImportGenesis,
    converted: &wire::SignedRecord,
    parents: &[ThreadOperation],
) -> Result<DelegatedImport> {
    let (_, operation) = verify_native_operation(converted)?;
    let active = delegation.scope().body();
    if operation.publisher.as_slice() != active.job_public_key {
        return Err(Reject::KeyRole.into());
    }
    let binding = genesis.payload.binding.as_ref().ok_or(Reject::Canonical)?;
    let binding_digest = contract::signed_genesis_digest(binding)?;
    let genesis_id = genesis.genesis.id()?;
    if !active.branch_manifest.iter().any(|b| {
        b.genesis_authority_digest == binding_digest
            && b.limit
                .as_ref()
                .is_some_and(|b| b.genesis_digest == genesis_id.as_bytes())
    }) {
        return Err(Reject::Scope.into());
    }
    Ok(DelegatedImport::bind(
        signed,
        delegation.scope(),
        &genesis.genesis,
        binding
            .body
            .as_ref()
            .and_then(|b| b.identity.as_ref())
            .ok_or(Reject::Canonical)?,
        &operation,
        parents,
    )?)
}

/// Bind only originals selected by an authenticated import signature. Other
/// originals retain strict native ancestry; no caller-supplied kind grants it.
pub fn bind_import_original(
    bundle: &wire::ImportPublicProofBundleV1,
    delegations: &[&contract::VerifiedImportDelegation],
    genesis: &ThreadGenesis,
    operation: &ThreadOperation,
    parents: &[ThreadOperation],
) -> Result<Option<DelegatedImport>> {
    let frontier = contract::frontier_digest(&wire::ImportFrontierV1 {
        format_version: 1,
        thread_id: operation.thread.as_bytes().to_vec(),
        operation_ids: vec![operation.id()?.as_bytes().to_vec()],
    })?;
    let Some(signed) = bundle.operations.iter().find(|signed| {
        signed
            .body
            .as_ref()
            .is_some_and(|body| body.resulting_frontier_digest == frontier)
    }) else {
        return Ok(None);
    };
    let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
    let delegation = delegations
        .iter()
        .find(|d| d.digest() == body.delegation_digest)
        .ok_or(Reject::Scope)?;
    let genesis_id = genesis.id()?;
    let binding = bundle
        .genesis_authorities
        .iter()
        .find(|g| {
            g.body
                .as_ref()
                .is_some_and(|g| g.genesis_digest == genesis_id.as_bytes())
        })
        .ok_or(Reject::GenesisBinding)?;
    let identity = binding
        .body
        .as_ref()
        .and_then(|g| g.identity.as_ref())
        .ok_or(Reject::GenesisBinding)?;
    Ok(Some(DelegatedImport::bind(
        signed, delegation, genesis, identity, operation, parents,
    )?))
}

/// Authenticated certificates awaiting exact native content. This grants only
/// structural import binding; durable installation rechecks owner/witness trust.
#[derive(Clone)]
pub struct VerifiedImportCarriers {
    bundle: wire::ImportPublicProofBundleV1,
    delegation: contract::VerifiedImportDelegation,
}
impl VerifiedImportCarriers {
    pub fn new(
        bundle: wire::ImportPublicProofBundleV1,
        delegation: contract::VerifiedImportDelegation,
    ) -> Result<Self> {
        contract::validate_public_bundle(&bundle)?;
        if !bundle.delegations.first().is_some_and(|signed| {
            contract::signed_delegation_digest(signed)
                .is_ok_and(|digest| digest == delegation.digest())
        }) {
            return Err(Reject::Scope.into());
        }
        Ok(Self { bundle, delegation })
    }
    pub fn bind(
        &self,
        genesis: &ThreadGenesis,
        operation: &ThreadOperation,
        parents: &[ThreadOperation],
    ) -> Result<Option<DelegatedImport>> {
        bind_import_original(
            &self.bundle,
            &[&self.delegation],
            genesis,
            operation,
            parents,
        )
    }
    pub fn bundle(&self) -> &wire::ImportPublicProofBundleV1 {
        &self.bundle
    }
}

/// Opaque original creation authority. Later owner transfers do not
/// replace its creator, account, native signature or first signed binding.
#[derive(Clone, Debug)]
pub struct VerifiedImportGenesis {
    payload: wire::ImportGenesisWitnessV1,
    original: SignedGenesis,
    genesis: ThreadGenesis,
}
impl VerifiedImportGenesis {
    pub fn original(&self) -> &SignedGenesis {
        &self.original
    }
    pub fn genesis(&self) -> &ThreadGenesis {
        &self.genesis
    }
    pub fn payload(&self) -> &wire::ImportGenesisWitnessV1 {
        &self.payload
    }
}

/// Independent committed-publication testimony; never job-key executor trust.
pub fn verify_publication(
    content: &DelegatedImport,
    delegation: &VerifiedImportDelegation,
    manifest: &wire::ImportResultManifestV1,
    evidence: &WitnessEvidence,
    set: &VerifiedWitnessSet,
    now_ms: i64,
) -> Result<()> {
    evidence.recheck(set, now_ms)?;
    contract::verify_publication(
        content.signed(),
        delegation.scope(),
        manifest,
        evidence.signed(),
        set,
        evidence.proof(),
        now_ms,
    )?;
    Ok(())
}

pub fn verify_genesis_payload(
    payload: &wire::ImportGenesisWitnessV1,
    evidence: &WitnessEvidence,
    delegation: &VerifiedImportDelegation,
    admitted_mint_roots: &[wire::SignedOwnerMintRootAttachment],
    is_revoked_at_accepted_order: impl Fn(
        heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool,
) -> Result<VerifiedImportGenesis> {
    verify_genesis_payload_inner(
        payload,
        evidence,
        delegation,
        admitted_mint_roots,
        false,
        is_revoked_at_accepted_order,
    )
}
fn verify_genesis_payload_inner(
    payload: &wire::ImportGenesisWitnessV1,
    evidence: &WitnessEvidence,
    delegation: &VerifiedImportDelegation,
    admitted_mint_roots: &[wire::SignedOwnerMintRootAttachment],
    boundary: bool,
    is_revoked_at_accepted_order: impl Fn(
        heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool,
) -> Result<VerifiedImportGenesis> {
    let original = payload.original_genesis.as_ref().ok_or(Reject::Canonical)?;
    let (signed, genesis) = verify_native_genesis(original)?;
    let s = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    contract::verify_witness_payload(s, WitnessPayload::Genesis(payload))?;
    let binding = payload.binding.as_ref().ok_or(Reject::Canonical)?;
    if s.basis == 2 && !boundary {
        return Err(Reject::BoundaryAcceptance.into());
    }
    if boundary {
        // Current accepting authority was verified against the complete selection.
    } else if let Some(permission) = delegation.member_permission() {
        let mut expected = b"heddle-signed-import-member-permission-v1\0".to_vec();
        expected.extend_from_slice(&api::hybrid_codec::canonical(permission)?);
        if expected != payload.creator_authority_envelope {
            return Err(Reject::ImportPermission.into());
        }
    } else {
        let id = delegation
            .scope()
            .body()
            .identity
            .as_ref()
            .ok_or(Reject::Canonical)?;
        let account: [u8; 16] = id
            .owner_account_uuid
            .as_slice()
            .try_into()
            .map_err(|_| Reject::Canonical)?;
        // Direct active owner signing still preserves independent original
        // account-native creation authority in its published format.
        heddleco_capability_verifier::thread_control_authority::verify_genesis_with_retained_mint_roots(
            &payload.creator_authority_envelope,
            heddleco_capability_verifier::thread_control_authority::Context {owner:delegation.owner(),account_uuid:&account,publisher:&genesis.creator,agent_id:None,
                method:"/heddle.api.v1alpha2.IntegrationService/ImportSource",spool_path:delegation.spool_path(),now:s.observed_at_unix_millis/1000},admitted_mint_roots,is_revoked_at_accepted_order)?;
    }
    contract::verify_genesis_authority(
        binding,
        delegation.scope(),
        genesis.id()?.as_bytes(),
        &signed.signature,
        &api::hybrid_codec::hash(&[&payload.creator_authority_envelope]),
    )?;
    let id = delegation
        .scope()
        .body()
        .identity
        .as_ref()
        .ok_or(Reject::Canonical)?;
    if genesis.owner
        != heddle_object_model::object::thread_replication::GenesisOwner::Account(
            uuid::Uuid::from_slice(&id.owner_account_uuid).map_err(|_| Reject::Canonical)?,
        )
        || genesis.creator.as_slice() != delegation.scope().body().delegating_public_key
        || genesis.spool
            != uuid::Uuid::from_slice(&id.spool_uuid)
                .map_err(|_| Reject::Canonical)?
                .to_string()
    {
        return Err(Reject::Root.into());
    }
    Ok(VerifiedImportGenesis {
        payload: payload.clone(),
        original: signed,
        genesis,
    })
}

/// Native causal/ownership closure. It supplies no account authorization;
/// each original account operation still needs its own admission evidence.
pub struct NativeClosure {
    geneses: std::collections::BTreeMap<heddle_object_model::object::ContentHash, ThreadGenesis>,
    operations:
        std::collections::BTreeMap<heddle_object_model::object::ContentHash, ThreadOperation>,
    claims: std::collections::BTreeMap<
        heddle_object_model::object::ContentHash,
        heddle_object_model::object::thread_replication::ownership_claim::ThreadOwnershipClaim,
    >,
    import_bound: std::collections::BTreeMap<heddle_object_model::object::ContentHash, DelegatedImport>,
    foreign: std::collections::BTreeSet<heddle_object_model::object::ContentHash>,
    authorized: std::collections::BTreeMap<heddle_object_model::object::ContentHash, u64>,
    resolutions: std::collections::BTreeMap<heddle_object_model::object::ContentHash, heddle_object_model::object::thread_replication::ownership_resolution::ThreadOwnershipResolution>,
}
impl NativeClosure {
    pub fn verify(records: &[wire::SignedRecord]) -> Result<Self> {
        Self::verify_with_boundaries(records, &[])
    }
    // A later claim cannot retroactively cover a landing's native LocalKey
    // source. Evaluate the exact signed ownership closure at that P4 order.
    fn verify_local_work(
        &self,
        id: &heddle_object_model::object::ContentHash,
        order: u64,
    ) -> Result<()> {
        use heddle_object_model::object::thread_replication::GenesisOwner;
        let op = self.operation(id)?;
        if self.genesis(&op.thread)?.owner != GenesisOwner::LocalKey(op.publisher) {
            return Err(Reject::Scope.into());
        }
        let admitted = |id| {
            self.authorized
                .get(&id)
                .is_some_and(|cutoff| *cutoff <= order)
        };
        let claims = self
            .claims
            .iter()
            .filter(|(id, c)| c.thread == op.thread && admitted(**id))
            .collect::<std::collections::BTreeMap<_, _>>();
        let resolutions = self
            .resolutions
            .iter()
            .filter(|(id, r)| r.thread == op.thread && admitted(**id))
            .map(|(_, r)| r)
            .collect::<Vec<_>>();
        let mut pending = match resolutions.as_slice() {
            [resolution]
                if resolution.conflicting_claims == claims.keys().map(|id| **id).collect()
                    && claims.contains_key(&resolution.winning_claim) =>
            {
                resolution.frontier.iter().copied().collect::<Vec<_>>()
            }
            [] if claims.len() == 1 => claims
                .values()
                .next()
                .ok_or(Reject::Scope)?
                .source_frontier
                .iter()
                .copied()
                .collect(),
            _ => return Err(Reject::Scope.into()),
        };
        let mut covered = std::collections::BTreeSet::new();
        while let Some(next) = pending.pop() {
            let ancestor = self.operation(&next)?;
            if ancestor.thread != op.thread {
                return Err(Reject::Scope.into());
            }
            if covered.insert(next) {
                pending.extend(ancestor.parents.iter().copied());
            }
        }
        if !covered.contains(id) {
            return Err(Reject::Scope.into());
        }
        Ok(())
    }

    /// Native boundary dependencies require the exact API-validated evidence.
    /// This preserves signatures/canonicality; account authority is checked at
    /// the authenticated witness observation by the payload verifier.
    pub fn verify_with_boundaries(
        records: &[wire::SignedRecord],
        boundaries: &[wire::ImportBoundaryAcceptanceV1],
    ) -> Result<Self> {
        Self::verify_with_imports(records, boundaries, |_, _, _| Ok(None))
    }

    /// Import ancestry requires an opaque carrier bound to the exact original.
    /// Without one this retains ordinary native validation, including roots.
    pub fn verify_with_imports(
        records: &[wire::SignedRecord],
        boundaries: &[wire::ImportBoundaryAcceptanceV1],
        import: impl Fn(
            &ThreadGenesis,
            &ThreadOperation,
            &[ThreadOperation],
        ) -> Result<Option<DelegatedImport>>,
    ) -> Result<Self> {
        Self::verify_with_resolvers(records, boundaries, import, |_| Ok(None))
    }

    /// The receiver resolver supplies only exact originals previously installed
    /// through the other origin. References alone never select this exception.
    pub fn verify_with_resolvers(
        records: &[wire::SignedRecord],
        boundaries: &[wire::ImportBoundaryAcceptanceV1],
        import: impl Fn(
            &ThreadGenesis,
            &ThreadOperation,
            &[ThreadOperation],
        ) -> Result<Option<DelegatedImport>>,
        foreign: impl Fn(&wire::SignedRecord) -> Result<Option<ForeignOriginal>>,
    ) -> Result<Self> {
        for boundary in boundaries {
            contract::verify_boundary_acceptance(boundary)?;
        }
        // Individual records retain their wire bounds. The caller stages
        // closure pages and chooses history depth; admission adds no ceiling.
        use heddle_object_model::object::thread_replication::{
            self as native, ownership_claim::ThreadOwnershipClaim,
            ownership_resolution::ThreadOwnershipResolution,
        };
        let mut result = Self {
            geneses: Default::default(),
            operations: Default::default(),
            claims: Default::default(),
            foreign: Default::default(),
            authorized: Default::default(),
            import_bound: Default::default(),
            resolutions: Default::default(),
        };
        let mut seen = std::collections::BTreeMap::new();
        for record in records {
            let digest = contract::signed_native_digest(record)?;
            if let Some(old) = seen.insert(digest, record) {
                if old != record {
                    return Err(Reject::Canonical.into());
                }
                continue;
            }
            if let Some(installed) = foreign(record)? {
                if installed.original != *record {
                    return Err(Reject::Scope.into());
                }
                let genesis_id = installed.genesis.id()?;
                result.geneses.insert(genesis_id, installed.genesis);
                let id = match record.format.as_str() {
                    native::GENESIS_FORMAT => verify_native_genesis(record)?.1.id()?,
                    native::OPERATION_FORMAT => {
                        let op = verify_native_operation(record)?.1;
                        if op.thread != genesis_id {
                            return Err(Reject::Scope.into());
                        }
                        let id = op.id()?;
                        if let Some(bound) = installed.import {
                            if bound.converted() != &op {
                                return Err(Reject::Scope.into());
                            }
                            result.import_bound.insert(id, bound);
                        }
                        result.operations.insert(id, op);
                        id
                    }
                    native::ownership_claim::FORMAT => {
                        let c = ThreadOwnershipClaim::decode(&record.canonical_record)?;
                        if record.signatures.len() != 2 {
                            return Err(Reject::Signature.into());
                        }
                        let c = crate::thread_ownership_claim::SignedOwnershipClaim {
                            canonical: record.canonical_record.clone(),
                            local_signature: record_signature(record, &c.prior_local_key)?,
                            acceptance_signature: record_signature(record, &c.accepting_publisher)?,
                        }
                        .verify()?;
                        c.validate_genesis(result.geneses.get(&genesis_id).ok_or(Reject::Scope)?)?;
                        let id = c.id()?;
                        result.claims.insert(id, c);
                        id
                    }
                    native::ownership_resolution::FORMAT => {
                        let r = ThreadOwnershipResolution::decode(&record.canonical_record)?;
                        if record.signatures.len() != 2 {
                            return Err(Reject::Signature.into());
                        }
                        r.validate_genesis(result.geneses.get(&genesis_id).ok_or(Reject::Scope)?)?;
                        let signing = crate::thread_ownership_resolution::signing_bytes(
                            &record.canonical_record,
                        );
                        crate::Ed25519Signer::verify_with_public_key(
                            &signing,
                            &r.local_owner,
                            &record_signature(record, &r.local_owner)?,
                        )?;
                        crate::Ed25519Signer::verify_with_public_key(
                            &signing,
                            &r.accepting_publisher,
                            &record_signature(record, &r.accepting_publisher)?,
                        )?;
                        let id = r.id()?;
                        result.resolutions.insert(id, r);
                        id
                    }
                    _ => return Err(Reject::Scope.into()),
                };
                result.foreign.insert(id);
                continue;
            }
            match record.format.as_str() {
                native::GENESIS_FORMAT => {
                    let (_, g) = verify_native_genesis(record)?;
                    result.geneses.insert(g.id()?, g);
                }
                native::OPERATION_FORMAT => {
                    let (_, o) = verify_native_operation(record)?;
                    result.operations.insert(o.id()?, o);
                }
                native::ownership_claim::FORMAT => {
                    let c = ThreadOwnershipClaim::decode(&record.canonical_record)?;
                    let signed = crate::thread_ownership_claim::SignedOwnershipClaim {
                        canonical: record.canonical_record.clone(),
                        local_signature: record_signature(record, &c.prior_local_key)?,
                        acceptance_signature: record_signature(record, &c.accepting_publisher)?,
                    };
                    if record.signatures.len() != 2 {
                        return Err(Reject::Signature.into());
                    }
                    let c = signed.verify()?;
                    result.claims.insert(c.id()?, c);
                }
                native::ownership_resolution::FORMAT => {
                    ThreadOwnershipResolution::decode(&record.canonical_record)?;
                }
                "heddle-original-boundary-acceptance-v1"
                | "heddle-thread-genesis-admission-v2"
                | "heddle-thread-authority-admission-v3" => {
                    if !boundaries.iter().any(|e| {
                        e.signed_acceptance.as_ref() == Some(record)
                            || e.original_receipts.contains(record)
                    }) {
                        return Err(Reject::BoundaryAcceptance.into());
                    }
                }
                _ => return Err(Reject::Version.into()),
            }
        }
        for op in result.operations.values() {
            if result.foreign.contains(&op.id()?) {
                continue;
            }
            let genesis = result.geneses.get(&op.thread).ok_or(Reject::Scope)?;
            let parents = op
                .parents
                .iter()
                .map(|id| result.operations.get(id).cloned().ok_or(Reject::Scope))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if let Some(bound) = import(genesis, op, &parents)? {
                if bound.converted() != op {
                    return Err(Reject::Scope.into());
                }
                bound.validate_parents(genesis, &parents)?;
                result.import_bound.insert(op.id()?, bound);
            } else {
                op.validate_parents(genesis, &parents)?;
            }
        }
        for c in result.claims.values() {
            if result.foreign.contains(&c.id()?) {
                continue;
            }
            c.validate_genesis(result.geneses.get(&c.thread).ok_or(Reject::Scope)?)?;
            for id in &c.source_frontier {
                if !result.operations.get(id).is_some_and(|o| {
                    o.thread == c.thread && o.source_state().ok().flatten().is_some()
                }) {
                    return Err(Reject::Scope.into());
                }
            }
        }
        for record in records
            .iter()
            .filter(|r| r.format == native::ownership_resolution::FORMAT)
        {
            let r = ThreadOwnershipResolution::decode(&record.canonical_record)?;
            if result.foreign.contains(&r.id()?) {
                continue;
            }
            r.validate_genesis(result.geneses.get(&r.thread).ok_or(Reject::Scope)?)?;
            if record.signatures.len() != 2 {
                return Err(Reject::Signature.into());
            }
            for id in &r.conflicting_claims {
                if !result.claims.get(id).is_some_and(|c| c.thread == r.thread) {
                    return Err(Reject::Scope.into());
                }
            }
            for id in &r.frontier {
                if !result.operations.get(id).is_some_and(|o| {
                    o.thread == r.thread && o.source_state().ok().flatten().is_some()
                }) {
                    return Err(Reject::Scope.into());
                }
            }
            let verified = crate::thread_ownership_resolution::SignedOwnershipResolution {
                canonical: record.canonical_record.clone(),
                local_signature: record_signature(record, &r.local_owner)?,
                acceptance_signature: record_signature(record, &r.accepting_publisher)?,
            }
            .verify(result.claims.get(&r.winning_claim).ok_or(Reject::Scope)?)?;
            result.resolutions.insert(verified.id()?, verified);
        }
        Ok(result)
    }
    pub fn import_bound(
        &self,
        id: &heddle_object_model::object::ContentHash,
    ) -> Option<&DelegatedImport> {
        self.import_bound.get(id)
    }
    pub fn foreign(&self, id: &heddle_object_model::object::ContentHash) -> bool {
        self.foreign.contains(id)
    }
    pub fn genesis(&self, id: &heddle_object_model::object::ContentHash) -> Result<&ThreadGenesis> {
        self.geneses.get(id).ok_or(Reject::Scope.into())
    }
    pub fn operation(
        &self,
        id: &heddle_object_model::object::ContentHash,
    ) -> Result<&ThreadOperation> {
        self.operations.get(id).ok_or(Reject::Scope.into())
    }
}
fn record_signature(record: &wire::SignedRecord, key: &[u8]) -> Result<Vec<u8>> {
    if record
        .signatures
        .windows(2)
        .any(|w| w[0].public_key >= w[1].public_key)
    {
        return Err(Reject::Canonical.into());
    }
    Ok(record
        .signatures
        .iter()
        .find(|s| s.public_key == key)
        .ok_or(Reject::Signature)?
        .signature
        .clone())
}

/// Accepted native owner context. Policy and disclosure are independently
/// selected caller gates; witness observation authorizes only exact history.
#[derive(Default)]
pub struct OriginalGeneses(
    std::collections::BTreeMap<
        heddle_object_model::object::ContentHash,
        (wire::ForeignDependencyOrigin, Vec<u8>),
    >,
);
impl OriginalGeneses {
    pub fn import(payloads: &[wire::ImportGenesisWitnessV1]) -> Result<Self> {
        let mut result = Self::default();
        for p in payloads {
            result.insert(
                p.original_genesis.as_ref().ok_or(Reject::Scope)?,
                &p.creator_authority_envelope,
                wire::ForeignDependencyOrigin::Import,
            )?;
        }
        Ok(result)
    }
    pub fn native(payloads: &[wire::NativeGenesisWitnessV1]) -> Result<Self> {
        let mut result = Self::default();
        for p in payloads {
            result.insert(
                p.original_genesis.as_ref().ok_or(Reject::Scope)?,
                &p.creator_authority_envelope,
                wire::ForeignDependencyOrigin::Native,
            )?;
        }
        Ok(result)
    }
    pub fn insert(
        &mut self,
        original: &wire::SignedRecord,
        envelope: &[u8],
        origin: wire::ForeignDependencyOrigin,
    ) -> Result<()> {
        let (_, genesis) = verify_native_genesis(original)?;
        let value = (origin, envelope.to_vec());
        if self
            .0
            .insert(genesis.id()?, value.clone())
            .is_some_and(|old| old != value)
        {
            return Err(Reject::Scope.into());
        }
        Ok(())
    }
    fn envelope(&self, id: &heddle_object_model::object::ContentHash) -> Result<&[u8]> {
        self.0
            .get(id)
            .map(|(_, e)| e.as_slice())
            .ok_or(Reject::BoundaryAcceptance.into())
    }
}
/// Exact installed foreign original, obtained under the receiver mutation lock.
#[derive(Clone)]
pub struct ForeignOriginal {
    pub original: wire::SignedRecord,
    pub genesis: ThreadGenesis,
    pub import: Option<DelegatedImport>,
}
pub struct NativeAuthorityContext<'a> {
    pub author_authority: &'a crate::writer_authority::WitnessedAuthors,
    pub owner: &'a heddleco_capability_verifier::VerifiedOwnerState,
    pub spool_uuid: uuid::Uuid,
    pub spool_genesis: &'a [u8; 32],
    pub transfer_sequence: u64,
    pub spool_path: &'a str,
    pub witness_set: &'a VerifiedWitnessSet,
    pub original_geneses: &'a OriginalGeneses,
    pub known_job_associations: &'a [(Vec<u8>, Vec<u8>)],
    pub forbidden_authority_keys: &'a [Vec<u8>],
}
type BoundaryCoverage = std::collections::BTreeMap<
    Vec<u8>,
    std::collections::BTreeSet<
        heddle_object_model::object::original_boundary_acceptance::ManifestSubject,
    >,
>;
/// Verify complete native selection and current accepting authority for every
/// acceptance, including dependencies whose acceptance differs from the original.
fn verify_boundary_selection(
    boundaries: &[wire::ImportBoundaryAcceptanceV1],
    statement: &host::HostedWitnessStatementV1,
    closure: &NativeClosure,
    context: &NativeAuthorityContext<'_>,
    revoked: &impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<BoundaryCoverage> {
    use heddle_object_model::object::{
        original_boundary_acceptance::{
            ManifestSubject, OriginalBoundaryAcceptance, OriginalManifestEntry,
            OriginalPublicationManifest, PublicationIntent,
        },
        thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission},
        thread_genesis_admission::ThreadGenesisAdmission,
        thread_replication::{SourceAuthor, integration::TrustedHostedExecutor},
    };
    use heddleco_capability_verifier::boundary_authority::{
        self, BoundarySubjectKind, OriginalSubjectScope,
    };
    if statement.spool_uuid != context.spool_uuid.as_bytes()
        || statement.spool_genesis_digest != context.spool_genesis
        || statement.owner_id != context.owner.owner_id()
        || statement.owner_state_hash != context.owner.state_hash()
        || statement.ownership_transfer_sequence != context.transfer_sequence
    {
        return Err(Reject::Root.into());
    }
    let issuer = context
        .witness_set
        .body()
        .entries
        .iter()
        .find(|e| e.executor_id == statement.executor_id)
        .ok_or(Reject::Signature)?;
    let executor: [u8; 32] = issuer
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| Reject::Canonical)?;
    let mut coverage = std::collections::BTreeMap::new();
    for e in boundaries {
        contract::verify_boundary_acceptance(e)?;
        let binding = e.binding.as_ref().ok_or(Reject::BoundaryAcceptance)?;
        let original = e
            .signed_acceptance
            .as_ref()
            .ok_or(Reject::BoundaryAcceptance)?;
        let body = OriginalBoundaryAcceptance::decode(&original.canonical_record)?;
        if original.signatures.len() != 1 {
            return Err(Reject::Signature.into());
        }
        let acceptance = crate::original_boundary_acceptance::SignedBoundaryAcceptance {
            canonical: original.canonical_record.clone(),
            signature: record_signature(original, &body.accepting_publisher)?,
        }
        .verify_signature()?;
        let manifest = OriginalPublicationManifest::decode(&e.originals_manifest)?;
        let intent = PublicationIntent::decode(&e.publication_intent)?;
        if intent.spool != context.spool_uuid
            || intent.spool_genesis.as_bytes() != context.spool_genesis
        {
            return Err(Reject::BoundaryAcceptance.into());
        }
        let selected = acceptance.selected(&intent, &manifest)?;
        if selected.len() != e.original_receipts.len() {
            return Err(Reject::BoundaryAcceptance.into());
        }
        let SourceAuthor::Account {
            actor,
            authority,
            spool,
            ..
        } = &acceptance.accepting_author
        else {
            return Err(Reject::BoundaryAcceptance.into());
        };
        if *spool != context.spool_uuid {
            return Err(Reject::BoundaryAcceptance.into());
        }
        let author = context.author_authority.resolve(
            actor.principal_id.as_bytes(),
            authority,
            context.owner,
            statement.observed_at_unix_millis / 1000,
        )?;
        api::writer_authority::verify_account_binding(
            &api::writer_authority::decode_authority(authority)?,
            actor.principal_id.as_bytes(),
            &context
                .owner
                .signed_root()
                .root
                .as_ref()
                .ok_or(Reject::Root)?
                .account_uuid,
            &context.owner.owner_id(),
        )?;
        let role_forbidden = |key: &[u8]| {
            context.forbidden_authority_keys.iter().any(|k| k == key)
                || context
                    .witness_set
                    .body()
                    .entries
                    .iter()
                    .any(|e| e.public_key == key)
                || context.known_job_associations.iter().any(|(k, _)| k == key)
        };
        if role_forbidden(&acceptance.accepting_publisher)
            || role_forbidden(
                &api::writer_authority::decode_authority(authority)?.mint_root_public_key,
            )
            || author
                .owner
                .authority_public_keys()
                .any(|key| role_forbidden(&key))
        {
            return Err(Reject::KeyRole.into());
        }
        let trust = TrustedHostedExecutor {
            spool: intent.spool,
            spool_genesis: intent.spool_genesis,
            executor,
        };
        let mut seen = std::collections::BTreeSet::new();
        for receipt in &e.original_receipts {
            if receipt.signatures.len() != 1
                || receipt.signatures[0].public_key != issuer.public_key
            {
                return Err(Reject::BoundaryAcceptance.into());
            }
            let subject = match receipt.format.as_str() {
                "heddle-thread-genesis-admission-v2" => ManifestSubject::Genesis(
                    ThreadGenesisAdmission::decode(&receipt.canonical_record)?.thread,
                ),
                "heddle-thread-authority-admission-v3" => {
                    match ThreadAuthorityAdmission::decode(&receipt.canonical_record)?.subject {
                        OriginalAuthoritySubject::Operation(id) => ManifestSubject::Source(id),
                        OriginalAuthoritySubject::OwnershipClaim(id) => {
                            ManifestSubject::OwnershipClaim(id)
                        }
                        OriginalAuthoritySubject::OwnershipResolution(id) => {
                            ManifestSubject::OwnershipResolution(id)
                        }
                    }
                }
                _ => return Err(Reject::BoundaryAcceptance.into()),
            };
            let entry = selected
                .iter()
                .find(|e| e.subject == subject)
                .ok_or(Reject::BoundaryAcceptance)?;
            if !seen.insert(subject.clone()) {
                return Err(Reject::BoundaryAcceptance.into());
            }
            if role_forbidden(&entry.publisher) {
                return Err(Reject::KeyRole.into());
            }
            let (descriptor, kind, method) = match &subject {
                ManifestSubject::Genesis(id) => {
                    let genesis = closure.genesis(id)?;
                    let envelope = context.original_geneses.envelope(id)?;
                    let receipt_body = crate::thread_genesis_admission::SignedGenesisAdmission {
                        canonical: receipt.canonical_record.clone(),
                        signature: receipt.signatures[0].signature.clone(),
                        boundary_acceptance: None,
                    }
                    .verify_signature()?;
                    receipt_body.authorize_with_acceptance(
                        genesis,
                        envelope,
                        &trust,
                        Some(&acceptance),
                    )?;
                    (
                        OriginalManifestEntry::from_genesis(genesis, envelope)?,
                        BoundarySubjectKind::AccountGenesis,
                        "/heddle.api.v1alpha2.ThreadService/StartThread",
                    )
                }
                ManifestSubject::Source(id) => {
                    let op = closure.operation(id)?;
                    let receipt_body =
                        crate::thread_authority_admission::SignedAuthorityAdmission {
                            canonical: receipt.canonical_record.clone(),
                            signature: receipt.signatures[0].signature.clone(),
                            boundary_acceptance: None,
                        }
                        .verify_signature()?;
                    receipt_body.authorize_with_acceptance(op, &trust, Some(&acceptance))?;
                    (
                        OriginalManifestEntry::from_operation(op)?,
                        BoundarySubjectKind::Source,
                        "/heddle.api.v1alpha2.SyncService/PublishContent",
                    )
                }
                ManifestSubject::OwnershipClaim(id) => {
                    let claim = closure.claims.get(id).ok_or(Reject::BoundaryAcceptance)?;
                    let receipt_body =
                        crate::thread_authority_admission::SignedAuthorityAdmission {
                            canonical: receipt.canonical_record.clone(),
                            signature: receipt.signatures[0].signature.clone(),
                            boundary_acceptance: None,
                        }
                        .verify_signature()?;
                    receipt_body.authorize_claim_with_acceptance(
                        claim,
                        closure.genesis(&claim.thread)?,
                        &trust,
                        Some(&acceptance),
                    )?;
                    (
                        OriginalManifestEntry::from_claim(claim)?,
                        BoundarySubjectKind::OwnershipClaim,
                        "/heddle.api.v1alpha2.ThreadService/ClaimThreadOwnership",
                    )
                }
                ManifestSubject::OwnershipResolution(id) => {
                    let resolution = closure
                        .resolutions
                        .get(id)
                        .ok_or(Reject::BoundaryAcceptance)?;
                    let receipt_body =
                        crate::thread_authority_admission::SignedAuthorityAdmission {
                            canonical: receipt.canonical_record.clone(),
                            signature: receipt.signatures[0].signature.clone(),
                            boundary_acceptance: None,
                        }
                        .verify_signature()?;
                    receipt_body.authorize_resolution_with_acceptance(
                        resolution,
                        closure.genesis(&resolution.thread)?,
                        &trust,
                        Some(&acceptance),
                    )?;
                    (
                        OriginalManifestEntry::from_resolution(resolution)?,
                        BoundarySubjectKind::OwnershipResolution,
                        "/heddle.api.v1alpha2.ThreadService/ResolveOwnershipConflict",
                    )
                }
                ManifestSubject::OtherOperation(_) => return Err(Reject::BoundaryAcceptance.into()),
            };
            if **entry != descriptor {
                return Err(Reject::BoundaryAcceptance.into());
            }
            boundary_authority::verify_accepting_authority(
                authority,
                heddleco_capability_verifier::thread_control_authority::Context {
                    owner: &author.owner,
                    account_uuid: actor.principal_id.as_bytes(),
                    publisher: &acceptance.accepting_publisher,
                    agent_id: actor.agent_id.as_deref(),
                    method,
                    spool_path: context.spool_path,
                    now: statement.observed_at_unix_millis / 1000,
                },
                OriginalSubjectScope {
                    kind,
                    account: acceptance.original_account.as_bytes(),
                    thread: entry.thread.as_bytes(),
                    subject: entry.subject.id().as_bytes(),
                    publisher: &entry.publisher,
                    agent_id: entry
                        .authority
                        .as_ref()
                        .and_then(|a| a.actor.agent_id.as_deref()),
                },
                &author.mint_roots,
                revoked,
            )?;
        }
        if !selected.iter().all(|e| seen.contains(&e.subject)) {
            return Err(Reject::BoundaryAcceptance.into());
        }
        if coverage
            .insert(binding.acceptance_id.clone(), seen)
            .is_some()
        {
            return Err(Reject::BoundaryAcceptance.into());
        }
    }
    Ok(coverage)
}

/// Genesis admission with exact boundary evidence and independently selected
/// accepting authority. OriginalAuthority retains its separate creator checks.
pub fn verify_genesis_payload_at_boundary(
    payload: &wire::ImportGenesisWitnessV1,
    evidence: &WitnessEvidence,
    delegation: &VerifiedImportDelegation,
    closure: &NativeClosure,
    context: &NativeAuthorityContext<'_>,
    revoked: impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<VerifiedImportGenesis> {
    let statement = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    contract::verify_witness_payload(statement, WitnessPayload::Genesis(payload))?;
    let boundaries: Vec<_> = payload.boundary_acceptance.iter().cloned().collect();
    let covered = verify_boundary_selection(&boundaries, statement, closure, context, &revoked)?;
    let boundary = if statement.basis == 2 {
        let binding = statement
            .boundary_acceptance
            .as_ref()
            .ok_or(Reject::BoundaryAcceptance)?;
        let (_, genesis) =
            verify_native_genesis(payload.original_genesis.as_ref().ok_or(Reject::Canonical)?)?;
        let genesis_id = genesis.id()?;
        if !covered.get(&binding.acceptance_id).is_some_and(|s| s.contains(&heddle_object_model::object::original_boundary_acceptance::ManifestSubject::Genesis(genesis_id))) {
            return Err(Reject::BoundaryAcceptance.into());
        }
        true
    } else {
        false
    };
    let roots = if boundary || delegation.member_permission().is_some() {
        vec![]
    } else {
        let (_, genesis) =
            verify_native_genesis(payload.original_genesis.as_ref().ok_or(Reject::Canonical)?)?;
        let heddle_object_model::object::thread_replication::GenesisOwner::Account(account) =
            genesis.owner
        else {
            return Err(Reject::GenesisBinding.into());
        };
        context
            .author_authority
            .resolve(
                account.as_bytes(),
                &payload.creator_authority_envelope,
                context.owner,
                statement.observed_at_unix_millis / 1000,
            )?
            .mint_roots
    };
    verify_genesis_payload_inner(payload, evidence, delegation, &roots, boundary, revoked)
}

/// Native genesis boundary acceptance retains every exact original and receipt.
pub(crate) fn verify_native_genesis_boundary(
    payload: &wire::NativeGenesisWitnessV1,
    evidence: &WitnessEvidence,
    closure: &NativeClosure,
    context: &NativeAuthorityContext<'_>,
    revoked: &impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<()> {
    let statement = evidence.signed().body.as_ref().ok_or(Reject::Canonical)?;
    let boundaries: Vec<_> = payload.boundary_acceptance.iter().cloned().collect();
    let covered = verify_boundary_selection(&boundaries, statement, closure, context, revoked)?;
    let binding = statement
        .boundary_acceptance
        .as_ref()
        .ok_or(Reject::BoundaryAcceptance)?;
    let (_, genesis) =
        verify_native_genesis(payload.original_genesis.as_ref().ok_or(Reject::Canonical)?)?;
    let id = genesis.id()?;
    if !covered.get(&binding.acceptance_id).is_some_and(|subjects| {
        subjects.contains(
            &heddle_object_model::object::original_boundary_acceptance::ManifestSubject::Genesis(
                id,
            ),
        )
    }) {
        return Err(Reject::BoundaryAcceptance.into());
    }
    Ok(())
}

fn native_authority(
    envelope: &[u8],
    publisher: &[u8; 32],
    actor: &heddle_object_model::object::CollaborationActor,
    method: &str,
    evidence: &WitnessEvidence,
    context: &NativeAuthorityContext<'_>,
    revoked: impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<()> {
    let author = context.author_authority.resolve(
        actor.principal_id.as_bytes(),
        envelope,
        context.owner,
        evidence
            .signed
            .body
            .as_ref()
            .ok_or(Reject::Canonical)?
            .observed_at_unix_millis
            / 1000,
    )?;
    let authority = api::writer_authority::decode_authority(envelope)?;
    api::writer_authority::verify_account_binding(
        &authority,
        actor.principal_id.as_bytes(),
        &context
            .owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid,
        &context.owner.owner_id(),
    )?;
    for key in std::iter::once(publisher.to_vec())
        .chain(std::iter::once(authority.mint_root_public_key.clone()))
        .chain(author.owner.authority_public_keys())
    {
        if context.forbidden_authority_keys.contains(&key)
            || context
                .known_job_associations
                .iter()
                .any(|(k, _)| k == &key)
        {
            return Err(Reject::KeyRole.into());
        }
    }
    let s = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    if s.spool_uuid != context.spool_uuid.as_bytes()
        || s.spool_genesis_digest != context.spool_genesis
        || s.owner_id != context.owner.owner_id()
        || s.owner_state_hash != context.owner.state_hash()
        || s.ownership_transfer_sequence != context.transfer_sequence
    {
        return Err(Reject::Root.into());
    }
    heddleco_capability_verifier::thread_control_authority::verify_with_retained_mint_roots(
        envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: &author.owner,
            account_uuid: actor.principal_id.as_bytes(),
            publisher,
            agent_id: actor.agent_id.as_deref(),
            method,
            spool_path: context.spool_path,
            now: s.observed_at_unix_millis / 1000,
        },
        &author.mint_roots,
        revoked,
    )?;
    Ok(())
}

/// Verify hosted landing's independent request, source, review and ancestry
/// originals. The caller supplies the accepted policy/frontier selection; an
/// import job permission cannot satisfy any of these account-authority gates.
pub fn verify_landing_payload(
    payload: &wire::HostedLandingWitnessV1,
    evidence: &WitnessEvidence,
    closure: &mut NativeClosure,
    context: &NativeAuthorityContext<'_>,
    revoked: impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<heddle_object_model::object::thread_replication::integration::HostedIntegration> {
    use api::v2::client::Rpc;
    use heddle_object_model::object::{
        ContentHash, StateId,
        thread_replication::{
            SourceAuthor, ThreadOperationBody,
            metadata::{Control, ThreadControl},
        },
    };
    let s = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    contract::verify_witness_payload(s, WitnessPayload::Landing(payload))?;
    let (_, execution) =
        verify_native_operation(payload.execution.as_ref().ok_or(Reject::Canonical)?)?;
    if closure.operation(&execution.id()?)? != &execution {
        return Err(Reject::Scope.into());
    }
    let integration = execution.integration()?.ok_or(Reject::ImportPermission)?;
    let (_, source) =
        verify_native_operation(payload.source_operation.as_ref().ok_or(Reject::Canonical)?)?;
    if closure.operation(&source.id()?)? != &source {
        return Err(Reject::Scope.into());
    }
    integration.validate_source(&source)?;
    if integration.spool != context.spool_uuid
        || integration.spool_genesis.as_bytes() != context.spool_genesis
        || witness_trust::witness_id(&integration.executor) != s.executor_id
        || integration.executed_at_ms != s.observed_at_unix_millis
    {
        return Err(Reject::Scope.into());
    }
    match source.source_author()? {
        Some(SourceAuthor::Account { spool, .. }) => {
            if spool != context.spool_uuid {
                return Err(Reject::Root.into());
            }
            if !closure.foreign(&source.id()?)
                && !closure
                    .authorized
                    .get(&source.id()?)
                    .is_some_and(|order| *order <= s.admission_order)
            {
                return Err(Reject::Scope.into());
            }
            // Exact P2 authority is checked at its own authenticated admission,
            // including boundary acceptance; later landing cannot renew it.
        }
        Some(SourceAuthor::LocalKey) => {
            // The installed carrier already binds the converted original to
            // its delegation job key. Only unbound native work needs ownership.
            if closure.import_bound(&source.id()?).is_none() && !closure.foreign(&source.id()?) {
                closure.verify_local_work(&source.id()?, s.admission_order)?;
            }
        }
        None if source.integration()?.is_some() => {
            if !closure.foreign(&source.id()?)
                && !closure
                    .authorized
                    .get(&source.id()?)
                    .is_some_and(|order| *order < s.admission_order)
            {
                return Err(Reject::Scope.into());
            }
        }
        _ => return Err(Reject::ImportPermission.into()),
    }
    let request = payload.request.as_ref().ok_or(Reject::Canonical)?;
    let body: wire::LandThreadRequest = hybrid_decode(&request.request_body)?;
    let signature = request.signature.as_ref().ok_or(Reject::Signature)?;
    let key: [u8; 32] = signature
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| Reject::Canonical)?;
    api::request_proof::verify_native_request_proof(
        &host::CallContext {
            client_operation_id: body.client_operation_id.clone(),
            request_proof: Some(host::RequestProof {
                algorithm: "ed25519".into(),
                signing_identity: request.signing_identity.clone(),
                timestamp_millis: request.timestamp_millis,
                nonce: request.nonce.clone(),
                signature: signature.signature.clone(),
            }),
            ..Default::default()
        },
        api::v2::rpc::ThreadServiceLandThread::METHOD,
        &request.request_body,
        &key,
        integration.executed_at_ms,
    )
    .map_err(|_| Reject::Signature)?;
    let mut preimage = api::signing::unary_bytes(
        &request.signing_identity,
        &request.method_path,
        request.timestamp_millis,
        &request.nonce,
        &request.request_body,
    );
    preimage.extend_from_slice(&signature.signature);
    if integration.initiating_request_proof
        != ContentHash::compute_typed("weft-hosted-landing-request-proof-v1", &preimage)
    {
        return Err(Reject::Scope.into());
    }
    use wire::revision_ref::Revision;
    // The unchanged execution attests frontier CAS; NativeClosure preserves
    // its complete target ancestry. A multi-head frontier is not necessarily
    // one State. Keep the user's exact signed selection for review matching.
    let Some(Revision::State(target)) = body
        .expected_target
        .as_ref()
        .and_then(|r| r.revision.as_ref())
    else {
        return Err(Reject::Scope.into());
    };
    let selected_target = StateId::from_bytes(
        target
            .value
            .as_slice()
            .try_into()
            .map_err(|_| Reject::Canonical)?,
    );
    if !body.thread.as_ref().is_some_and(|t| t.spool.as_ref().is_some_and(|v|v.id==integration.spool.to_string()) && t.id.as_ref().is_some_and(|id|id.value==integration.source_thread.as_bytes()))
        || !body.target.as_ref().is_some_and(|t|t.spool.as_ref().is_some_and(|v|v.id==integration.spool.to_string()) && t.id.as_ref().is_some_and(|id|id.value==integration.target_thread.as_bytes()))
        || !body.source.as_ref().is_some_and(|r|r.spool.as_ref().is_some_and(|v|v.id==integration.spool.to_string()) && matches!(&r.revision,Some(Revision::State(id)) if id.value==integration.source_revision.as_bytes()))
        || !body.expected_target.as_ref().is_some_and(|r|r.spool.as_ref().is_some_and(|v|v.id==integration.spool.to_string()) && matches!(&r.revision,Some(Revision::State(id)) if id.value==selected_target.as_bytes()))
        || body.expected_policy_version!=integration.review_policy_version.as_bytes()
    {return Err(Reject::Scope.into());}
    contract::verify_landing_key_roles(
        payload,
        &context
            .known_job_associations
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>(),
        context.forbidden_authority_keys,
    )?;
    let inspected =
        heddleco_capability_verifier::thread_control_authority::inspect_landing_subject(
            &payload.authority_envelope,
        )?;
    let account = inspected.account_uuid;
    let author = context.author_authority.resolve(
        &account,
        &payload.authority_envelope,
        context.owner,
        s.observed_at_unix_millis / 1000,
    )?;
    let verified = heddleco_capability_verifier::thread_control_authority::verify_landing_request_with_retained_mint_roots(
        &payload.authority_envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: &author.owner, account_uuid: &account, publisher: &key,
            agent_id: None, method: request.method_path.as_str(), spool_path: context.spool_path,
            now: s.observed_at_unix_millis / 1000,
        },
        &author.mint_roots, &revoked,
    )?;
    api::writer_authority::verify_landing_actor_binding(
        payload,
        &verified.account_uuid,
        &inspected.publisher,
        &key,
        &context
            .owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid,
        &context.owner.owner_id(),
    )?;
    let mut review_ids = std::collections::BTreeSet::new();
    for original in &payload.review_evidence {
        let (_, op) = verify_native_operation(original)?;
        if closure.operation(&op.id()?)? != &op || op.thread != integration.source_thread {
            return Err(Reject::Scope.into());
        }
        let ThreadOperationBody::Metadata(bytes) = &op.body else {
            return Err(Reject::Scope.into());
        };
        let control = ThreadControl::decode(bytes)?;
        if !closure.foreign(&op.id()?)
            && !closure
                .authorized
                .get(&op.id()?)
                .is_some_and(|order| *order <= s.admission_order)
        {
            return Err(Reject::Scope.into());
        }
        let Control::Review(review) = control.control else {
            return Err(Reject::Scope.into());
        };
        if review.source != integration.source_revision
            || review.target != selected_target
            || review.policy_version != integration.review_policy_version
            || review
                .expires_at_unix_seconds
                .is_some_and(|expiry| s.observed_at_unix_millis / 1000 >= expiry)
        {
            return Err(Reject::Scope.into());
        }
        review_ids.insert(op.id()?);
    }
    if review_ids != integration.review_evidence {
        return Err(Reject::Scope.into());
    }
    closure
        .authorized
        .insert(execution.id()?, s.admission_order);
    Ok(integration)
}
fn hybrid_decode<T: prost::Message + Default>(bytes: &[u8]) -> Result<T> {
    Ok(api::hybrid_codec::strict_decode(
        bytes,
        contract::MAX_RECORD_BYTES,
    )?)
}

/// Original source/control or co-signed ownership authority, with exact native
/// causal closure. A witness cannot stand in for the original actor's grant.
pub fn verify_authority_payload(
    payload: &wire::ImportAuthorityWitnessV1,
    evidence: &WitnessEvidence,
    closure: &mut NativeClosure,
    context: &NativeAuthorityContext<'_>,
    revoked: impl Fn(heddleco_capability_verifier::thread_control_authority::Revocation<'_>) -> bool,
) -> Result<()> {
    use heddle_object_model::object::thread_replication::{
        SourceAuthor, ThreadOperationBody, metadata::ThreadControl,
        ownership_claim::ThreadOwnershipClaim, ownership_resolution::ThreadOwnershipResolution,
    };
    let s = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    contract::verify_witness_payload(s, WitnessPayload::Authority(payload))?;
    let original = payload.original.as_ref().ok_or(Reject::Canonical)?;
    let (publisher, actor, method, thread, envelope) = match payload.kind {
        1 => {
            let (_, op) = verify_native_operation(original)?;
            if closure.operation(&op.id()?)? != &op {
                return Err(Reject::Scope.into());
            }
            match &op.body {
                ThreadOperationBody::Metadata(bytes) => {
                    let c = ThreadControl::decode(bytes)?;
                    if c.spool != context.spool_uuid {
                        return Err(Reject::Root.into());
                    }
                    let method = c.authorization_method();
                    (
                        op.publisher,
                        c.actor,
                        method,
                        op.thread,
                        c.authority_envelope,
                    )
                }
                ThreadOperationBody::Capture(_) | ThreadOperationBody::LocalIntegration(_) => {
                    let Some(SourceAuthor::Account {
                        spool,
                        actor,
                        authority,
                        ..
                    }) = op.source_author()?
                    else {
                        return Err(Reject::ImportPermission.into());
                    };
                    if spool != context.spool_uuid {
                        return Err(Reject::Root.into());
                    }
                    (op.publisher,actor,heddle_object_model::object::thread_replication::SOURCE_AUTHORIZATION_METHOD,op.thread,authority)
                }
                _ => return Err(Reject::ImportPermission.into()),
            }
        }
        2 => {
            let c = ThreadOwnershipClaim::decode(&original.canonical_record)?;
            if closure.claims.get(&c.id()?) != Some(&c) {
                return Err(Reject::Scope.into());
            }
            let SourceAuthor::Account {
                spool,
                actor,
                authority,
                ..
            } = c.acceptance
            else {
                return Err(Reject::ImportPermission.into());
            };
            if spool != context.spool_uuid {
                return Err(Reject::Root.into());
            }
            (
                c.accepting_publisher,
                actor,
                "/heddle.api.v1alpha2.ThreadService/ClaimThreadOwnership",
                c.thread,
                authority,
            )
        }
        3 => {
            let r = ThreadOwnershipResolution::decode(&original.canonical_record)?;
            if closure.resolutions.get(&r.id()?) != Some(&r) {
                return Err(Reject::Scope.into());
            }
            let SourceAuthor::Account {
                spool,
                actor,
                authority,
                ..
            } = r.acceptance
            else {
                return Err(Reject::ImportPermission.into());
            };
            if spool != context.spool_uuid {
                return Err(Reject::Root.into());
            }
            (
                r.accepting_publisher,
                actor,
                "/heddle.api.v1alpha2.ThreadService/ResolveOwnershipConflict",
                r.thread,
                authority,
            )
        }
        _ => return Err(Reject::Version.into()),
    };
    if closure.genesis(&thread)?.spool != context.spool_uuid.to_string()
        || envelope != payload.authority_envelope
    {
        return Err(Reject::Scope.into());
    }
    api::writer_authority::verify_authority_actor_binding(
        payload,
        actor.principal_id.as_bytes(),
        &context
            .owner
            .signed_root()
            .root
            .as_ref()
            .ok_or(Reject::Root)?
            .account_uuid,
        &context.owner.owner_id(),
    )?;
    let coverage =
        verify_boundary_selection(&payload.boundary_acceptances, s, closure, context, &revoked)?;
    if s.basis == 2 {
        use heddle_object_model::object::original_boundary_acceptance::ManifestSubject;
        let subject = match payload.kind {
            1 => ManifestSubject::Source(verify_native_operation(original)?.1.id()?),
            2 => ManifestSubject::OwnershipClaim(
                ThreadOwnershipClaim::decode(&original.canonical_record)?.id()?,
            ),
            3 => ManifestSubject::OwnershipResolution(
                ThreadOwnershipResolution::decode(&original.canonical_record)?.id()?,
            ),
            _ => return Err(Reject::Version.into()),
        };
        let binding = s
            .boundary_acceptance
            .as_ref()
            .ok_or(Reject::BoundaryAcceptance)?;
        if !coverage
            .get(&binding.acceptance_id)
            .is_some_and(|c| c.contains(&subject))
        {
            return Err(Reject::BoundaryAcceptance.into());
        }
        closure.authorized.insert(
            heddle_object_model::object::ContentHash::compute_typed(
                &original.format,
                &original.canonical_record,
            ),
            s.admission_order,
        );
        return Ok(());
    }
    native_authority(
        &payload.authority_envelope,
        &publisher,
        &actor,
        method,
        evidence,
        context,
        revoked,
    )?;
    closure.authorized.insert(
        heddle_object_model::object::ContentHash::compute_typed(
            &original.format,
            &original.canonical_record,
        ),
        s.admission_order,
    );
    Ok(())
}
