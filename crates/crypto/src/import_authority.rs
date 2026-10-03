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

/// A typed fail-closed error; pending boundary binding is never inferred from
/// timestamps or from an unrelated valid witness signature.
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
    #[error("boundary acceptance witness binding requires api#318")]
    BoundaryAcceptancePendingApi318,
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
        crate::original_boundary_acceptance::require_hybrid_original_authority(signed)?;
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
/// `converter` is the original verified delegation whose job key signed the
/// retained native conversion; renewal never re-signs those original bytes.
pub fn verify_delegated_import(
    signed: &wire::SignedDelegatedImportOperationV1,
    delegation: &VerifiedImportDelegation,
    converter: &VerifiedImportDelegation,
    genesis: &VerifiedImportGenesis,
    converted: &wire::SignedRecord,
    parents: &[ThreadOperation],
) -> Result<DelegatedImport> {
    let (_, operation) = verify_native_operation(converted)?;
    let active = delegation.scope().body();
    let original = converter.scope().body();
    let active_id = active.identity.as_ref().ok_or(Reject::Canonical)?;
    let converter_id = original.identity.as_ref().ok_or(Reject::Canonical)?;
    if operation.publisher.as_slice() != original.job_public_key
        || active.logical_job_id != original.logical_job_id
        || active.retry_lineage_id != original.retry_lineage_id
        || active_id.spool_uuid != converter_id.spool_uuid
        || active_id.spool_genesis_digest != converter_id.spool_genesis_digest
    {
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

/// Opaque original creation authority. Later owner transfers/renewals do not
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
    is_revoked_at_accepted_order: impl Fn(
        heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool,
) -> Result<VerifiedImportGenesis> {
    let original = payload.original_genesis.as_ref().ok_or(Reject::Canonical)?;
    let (signed, genesis) = verify_native_genesis(original)?;
    let s = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    contract::verify_witness_payload(s, WitnessPayload::Genesis(payload))?;
    let binding = payload.binding.as_ref().ok_or(Reject::Canonical)?;
    if let Some(permission) = delegation.member_permission() {
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
                method:"/heddle.api.v1alpha2.IntegrationService/ImportSource",spool_path:delegation.spool_path(),now:s.observed_at_unix_millis/1000},&[],is_revoked_at_accepted_order)?;
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
    resolutions: std::collections::BTreeMap<heddle_object_model::object::ContentHash, heddle_object_model::object::thread_replication::ownership_resolution::ThreadOwnershipResolution>,
}
impl NativeClosure {
    pub fn verify(records: &[wire::SignedRecord]) -> Result<Self> {
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
                _ => return Err(Reject::Version.into()),
            }
        }
        for op in result.operations.values() {
            let genesis = result.geneses.get(&op.thread).ok_or(Reject::Scope)?;
            let parents = op
                .parents
                .iter()
                .map(|id| result.operations.get(id).cloned().ok_or(Reject::Scope))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            op.validate_parents(genesis, &parents)?;
        }
        for c in result.claims.values() {
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
pub struct NativeAuthorityContext<'a> {
    pub owner: &'a heddleco_capability_verifier::VerifiedOwnerState,
    pub spool_uuid: uuid::Uuid,
    pub spool_genesis: &'a [u8; 32],
    pub transfer_sequence: u64,
    pub spool_path: &'a str,
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
    let s = evidence.signed.body.as_ref().ok_or(Reject::Canonical)?;
    if s.spool_uuid != context.spool_uuid.as_bytes()
        || s.spool_genesis_digest != context.spool_genesis
        || s.owner_id != context.owner.owner_id()
        || s.owner_state_hash != context.owner.state_hash()
        || s.ownership_transfer_sequence != context.transfer_sequence
    {
        return Err(Reject::Root.into());
    }
    heddleco_capability_verifier::thread_control_authority::verify(
        envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: context.owner,
            account_uuid: actor.principal_id.as_bytes(),
            publisher,
            agent_id: actor.agent_id.as_deref(),
            method,
            spool_path: context.spool_path,
            now: s.observed_at_unix_millis / 1000,
        },
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
    closure: &NativeClosure,
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
        || integration.review_policy_version.as_bytes().as_slice() != s.policy_state_hash
    {
        return Err(Reject::Scope.into());
    }
    let ThreadOperationBody::Capture(capture) = &source.body else {
        return Err(Reject::ImportPermission.into());
    };
    let SourceAuthor::Account {
        actor,
        authority,
        spool,
        ..
    } = &capture.author
    else {
        return Err(Reject::ImportPermission.into());
    };
    if spool != &context.spool_uuid {
        return Err(Reject::Root.into());
    }
    native_authority(
        authority,
        &source.publisher,
        actor,
        heddle_object_model::object::thread_replication::SOURCE_AUTHORIZATION_METHOD,
        evidence,
        context,
        &revoked,
    )?;
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
    let root = context
        .owner
        .signed_root()
        .root
        .as_ref()
        .ok_or(Reject::Root)?;
    let account: [u8; 16] = root
        .account_uuid
        .as_slice()
        .try_into()
        .map_err(|_| Reject::Canonical)?;
    heddleco_capability_verifier::thread_control_authority::verify_landing_request(
        &payload.authority_envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: context.owner,
            account_uuid: &account,
            publisher: &key,
            agent_id: None,
            method: request.method_path.as_str(),
            spool_path: context.spool_path,
            now: s.observed_at_unix_millis / 1000,
        },
        &revoked,
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
        native_authority(
            &control.authority_envelope,
            &op.publisher,
            &control.actor,
            control.authorization_method(),
            evidence,
            context,
            &revoked,
        )?;
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
    closure: &NativeClosure,
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
            match op.body {
                ThreadOperationBody::Metadata(bytes) => {
                    let c = ThreadControl::decode(&bytes)?;
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
                ThreadOperationBody::Capture(c) => {
                    let SourceAuthor::Account {
                        spool,
                        actor,
                        authority,
                        ..
                    } = c.author
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
    native_authority(
        &payload.authority_envelope,
        &publisher,
        &actor,
        method,
        evidence,
        context,
        revoked,
    )
}
