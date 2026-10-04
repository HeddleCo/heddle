//! Part 2's verify-before-install seam for complete HYBRID public evidence.
//! Transport authenticates disclosure/delivery separately. This module retains
//! unchanged originals, resolves each witness under receiver-owned trust, and
//! commits genesis/content/proof/job associations in the same transaction.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec::{self, Reject},
    import_authority as contract,
};
use crypto::import_authority::{self as verification, NativeClosure, WitnessEvidence};
use heddleco_capability_verifier::import_delegation::{
    self as permission, CurrentContext, Selection, VerifiedImportDelegation,
};
use objects::{
    object::{
        ContentHash,
        thread_replication::{Admission, ThreadOperation},
    },
    store::ObjectStore,
};
use prost::Message;
use rusqlite::{OptionalExtension, params};

use super::{
    Error, Result, ThreadReplica,
    hosted_trust::{Clock, HostedTrust, TrustTransaction},
    install_artifacts::InstallArtifacts,
};

/// Receiver-owned accepted authority lookup. Implementations select verified
/// keyring/owner contexts from exact public histories and authenticated accepted
/// state/order, never author timestamps, carried self-PoP, or online roles.
/// Returned contexts are checked against the repository's independent pins.
pub trait AcceptedAuthority {
    /// Check current disclosure/audience/source access inside the durable
    /// mutation. Historical import permission does not grant today's access.
    fn authorize_import(
        &self,
        bundle: &wire::ImportPublicProofBundleV1,
        now_millis: i64,
        context: &TrustTransaction<'_>,
    ) -> Result<()>;
    fn authorize_native(
        &self,
        _bundle: &wire::NativePublicProofBundleV1,
        _now_millis: i64,
        _context: &TrustTransaction<'_>,
    ) -> Result<()> {
        Err(Error::WitnessEvidenceRequired)
    }
    fn for_native_binding(
        &self,
        binding: &wire::NativeGenesisAuthorityV1,
    ) -> Result<Selection<'_>> {
        let id = binding.identity.as_ref().ok_or(Reject::GenesisBinding)?;
        self.for_witness(&host::HostedWitnessStatementV1 {
            spool_uuid: id.spool_uuid.clone(),
            spool_genesis_digest: id.spool_genesis_digest.clone(),
            owner_id: id.owner_id.clone(),
            owner_state_hash: id.owner_state_hash.clone(),
            ownership_transfer_sequence: id.ownership_transfer_sequence,
            ..Default::default()
        })
    }
    fn for_witness(&self, statement: &host::HostedWitnessStatementV1) -> Result<Selection<'_>>;
    fn for_policy(&self, policy: &wire::SignedPolicyBody) -> Result<Selection<'_>>;
    fn import_revoked(
        &self,
        statement: &host::HostedWitnessStatementV1,
        revocation: permission::Revocation<'_>,
    ) -> bool;
    fn native_revoked(
        &self,
        statement: &host::HostedWitnessStatementV1,
        revocation: heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool;
}

/// Retained original witness sidecar. Refresh its proof/set separately when
/// exporting; the original statement and its signature must remain unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct HostedAdmission {
    pub deployment_authority: String,
    pub statement: host::SignedHostedWitnessStatementV1,
    pub proof: Option<host::HostedWitnessHistoryProofV1>,
}

pub(super) fn evidence(
    proofs: &[host::HostedWitnessHistoryProofV1],
    context: &TrustTransaction<'_>,
    signed: &host::SignedHostedWitnessStatementV1,
) -> Result<WitnessEvidence> {
    match WitnessEvidence::resolve(context.set(), signed, None, false, context.now_millis()) {
        Ok(e) => Ok(e),
        Err(verification::Error::Contract(Reject::Proof)) => {
            let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
            for proof in proofs {
                if proof.executor_id == body.executor_id
                    && proof.purpose == body.purpose
                    && let Ok(e) = WitnessEvidence::resolve(
                        context.set(),
                        signed,
                        Some(proof),
                        false,
                        context.now_millis(),
                    )
                {
                    return Ok(e);
                }
            }
            Err(Error::Hybrid(Reject::Proof))
        }
        Err(e) => Err(e.into()),
    }
}
fn current_context<'a>(
    selection: Selection<'a>,
    context: &'a TrustTransaction<'_>,
    forbidden: &'a [Vec<u8>],
    job_associations: &'a [(Vec<u8>, Vec<u8>)],
) -> CurrentContext<'a> {
    CurrentContext {
        selection,
        now_millis: context.now_millis(),
        forbidden_job_keys: forbidden,
        known_job_associations: job_associations,
    }
}
fn find_publication<'a>(
    bundle: &'a wire::ImportPublicProofBundleV1,
    operation: &wire::SignedDelegatedImportOperationV1,
) -> Result<(
    &'a wire::ImportResultManifestV1,
    &'a host::SignedHostedWitnessStatementV1,
)> {
    for manifest in &bundle.manifests {
        let payload =
            hybrid_codec::canonical(&contract::publication_payload(operation, manifest)?)?;
        if let Some(s) = bundle.statements.iter().find(|s| {
            s.body
                .as_ref()
                .is_some_and(|s| s.purpose == 3 && s.canonical_payload == payload)
        }) {
            return Ok((manifest, s));
        }
    }
    Err(Error::Hybrid(Reject::Scope))
}

/// Public lineage references shared by the two explicit evidence arms.
#[derive(Clone, Copy)]
pub struct PublicHistory<'a> {
    pub owner_genesis: Option<&'a wire::SignedSpoolOwnerGenesis>,
    pub owner_histories: &'a [wire::OwnerHistory],
    pub ownership_transfers: &'a [wire::ResourceTransferAuditRecord],
}
impl<'a> From<&'a wire::ImportPublicProofBundleV1> for PublicHistory<'a> {
    fn from(b: &'a wire::ImportPublicProofBundleV1) -> Self {
        Self {
            owner_genesis: b.owner_genesis.as_ref(),
            owner_histories: &b.owner_histories,
            ownership_transfers: &b.ownership_transfers,
        }
    }
}
impl<'a> From<&'a wire::NativePublicProofBundleV1> for PublicHistory<'a> {
    fn from(b: &'a wire::NativePublicProofBundleV1) -> Self {
        Self {
            owner_genesis: b.owner_genesis.as_ref(),
            owner_histories: &b.owner_histories,
            ownership_transfers: &b.ownership_transfers,
        }
    }
}

pub(super) fn public_owners(
    bundle: PublicHistory<'_>,
    now: i64,
) -> Result<BTreeMap<[u8; 32], heddleco_capability_verifier::VerifiedOwnerState>> {
    let mut states = BTreeMap::new();
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600)?;
    for history in bundle.owner_histories {
        let mut state = heddleco_capability_verifier::verify_owner_root(
            history.root.as_ref().ok_or(Reject::Root)?,
        )?;
        states.insert(state.state_hash(), state.clone());
        for transition in &history.accepted_transitions {
            state = heddleco_capability_verifier::apply_accepted_transition(
                &state, transition, now, limits,
            )?;
            states.insert(state.state_hash(), state.clone());
        }
        if history.state_hash != state.state_hash() {
            return Err(Error::Hybrid(Reject::Root));
        }
    }
    Ok(states)
}
pub(super) fn require_public_selection(
    bundle: PublicHistory<'_>,
    states: &BTreeMap<[u8; 32], heddleco_capability_verifier::VerifiedOwnerState>,
    selection: &Selection<'_>,
) -> Result<()> {
    if bundle.owner_genesis != Some(selection.keyring.owner_genesis().signed())
        || !bundle
            .ownership_transfers
            .starts_with(&selection.keyring.wire().ownership_transfers)
        || !states
            .get(&selection.owner.state_hash())
            .is_some_and(|state| state.signed_root() == selection.owner.signed_root())
        || !states
            .get(&selection.keyring.owner_state().state_hash())
            .is_some_and(|state| {
                state.signed_root() == selection.keyring.owner_state().signed_root()
            })
        || selection
            .keyring
            .wire()
            .transfer_owner_histories
            .iter()
            .any(|h| !bundle.owner_histories.contains(h))
    {
        return Err(Error::Hybrid(Reject::Root));
    }
    Ok(())
}

pub(super) fn verify_complete_transfer_history(
    bundle: PublicHistory<'_>,
    selection: &Selection<'_>,
    now: i64,
) -> Result<()> {
    let mut keyring = selection.keyring.wire().clone();
    keyring.ownership_transfers = bundle.ownership_transfers.to_vec();
    keyring.transfer_owner_histories = bundle.owner_histories.to_vec();
    // Authenticate the full history first. Historical selections below keep
    // their exact signed prefixes and original accepted owner state.
    let initial = bundle
        .owner_histories
        .iter()
        .filter(|h| h.root == keyring.owner_root)
        .max_by_key(|h| h.accepted_transitions.len())
        .ok_or(Reject::Root)?;
    keyring.accepted_transitions = initial.accepted_transitions.clone();
    keyring.accepted_state_hash = initial.state_hash.clone();
    heddleco_capability_verifier::verify_clone_keyring(keyring, now, selection.limits, &[])?;
    Ok(())
}

impl ThreadReplica {
    /// Read exact genesis or operation evidence for this Thread. This getter
    /// supplies no authority: receivers still resolve a fresh selected set.
    pub fn hosted_admission(&self, id: ContentHash) -> Result<Option<HostedAdmission>> {
        let row: Option<(String, Vec<u8>, Option<Vec<u8>>)> = self.connect()?.query_row(
            "SELECT authority,statement,proof FROM hosted_import_admissions WHERE operation=?1 AND (operation=?2 OR EXISTS(SELECT 1 FROM operations WHERE id=?1 AND thread=?2))",
            params![id.as_bytes(), self.thread.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        row.map(|(deployment_authority, statement, proof)| {
            Ok(HostedAdmission {
                deployment_authority,
                statement: hybrid_codec::strict_decode(&statement, 128 * 1024)?,
                proof: proof
                    .map(|bytes| {
                        hybrid_codec::strict_decode(&bytes, api::witness_trust::MAX_PROOF_BYTES)
                    })
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Verify the complete public history and install only selected originals
    /// and their causal dependencies. A selected genesis requires no conversion.
    /// The callback must install actual packs/pins/sidecars through its journal;
    /// any callback or commit-time rejection restores them under the trust lock.
    /// Public evidence includes the complete post-renewal history.
    /// Canonical protobuf is checked before trusting typed fields. `store` must
    /// be an isolated staging store: receives may write source states there.
    /// Publish its actual pack and sidecars through the callback journal.
    /// A fresh set and receiver clock are mandatory even for exact replay.
    pub fn install_hybrid_import(
        directory: &Path,
        trust: &HostedTrust<impl Clock>,
        bundle_bytes: &[u8],
        native_records: &[wire::SignedRecord],
        authority: &impl AcceptedAuthority,
        store: &impl ObjectStore,
        before_commit: impl FnOnce(&mut InstallArtifacts<'_>) -> Result<()>,
    ) -> Result<Vec<Self>> {
        Self::install_hybrid_import_with(
            directory,
            trust,
            bundle_bytes,
            native_records,
            authority,
            store,
            |_| Ok(()),
            |_, artifacts| before_commit(artifacts),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn install_hybrid_import_with(
        directory: &Path,
        trust: &HostedTrust<impl Clock>,
        bundle_bytes: &[u8],
        native_records: &[wire::SignedRecord],
        authority: &impl AcceptedAuthority,
        store: &impl ObjectStore,
        before_install: impl FnOnce(&TrustTransaction<'_>) -> Result<()>,
        before_commit: impl FnOnce(&TrustTransaction<'_>, &mut InstallArtifacts<'_>) -> Result<()>,
    ) -> Result<Vec<Self>> {
        if directory.canonicalize()? != trust.directory().canonicalize()? {
            return Err(Error::Hybrid(Reject::Root));
        }
        let bundle: wire::ImportPublicProofBundleV1 =
            hybrid_codec::strict_decode(bundle_bytes, contract::MAX_BUNDLE_BYTES)?;
        contract::validate_public_bundle(&bundle)?;
        if bundle.history_proofs.iter().any(|p| {
            p.encoded_len() > api::witness_trust::MAX_PROOF_BYTES
                || p.siblings.len() > api::witness_trust::MAX_SIBLINGS
        }) {
            return Err(Error::Hybrid(Reject::Bounds));
        }
        let signed_set = bundle.witness_set.as_ref().ok_or(Reject::Root)?;
        let replicas = trust.mutate_with_artifacts(
            signed_set,
            |context| {
                before_install(context)?;
                install_in(
                    directory,
                    &bundle,
                    native_records,
                    authority,
                    store,
                    context,
                )
            },
            |context, now| authority.authorize_import(&bundle, now, context),
            before_commit,
            |context, now| authority.authorize_import(&bundle, now, context),
        )?;
        if let Some(replica) = replicas.first() {
            replica.notify_committed()?;
        }
        Ok(replicas)
    }
    /// Export retained complete public dependencies. Witness-set freshness must
    /// be refreshed by Part 2 before a receiving mutation, without re-signing
    /// any original receipt, binding, certificate or converted operation.
    pub fn hybrid_import_bundle(&self) -> Result<Option<wire::ImportPublicProofBundleV1>> {
        let bytes: Option<Vec<u8>> = self
            .connect()?
            .query_row(
                "SELECT bundle FROM hosted_import_proofs WHERE thread=?1",
                [self.thread.as_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        bytes
            .map(|b| {
                hybrid_codec::strict_decode(&b, contract::MAX_BUNDLE_BYTES).map_err(Error::from)
            })
            .transpose()
    }
}
fn install_in(
    directory: &Path,
    bundle: &wire::ImportPublicProofBundleV1,
    native_records: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
    context: &TrustTransaction<'_>,
) -> Result<Vec<ThreadReplica>> {
    authority.authorize_import(bundle, context.now_millis(), context)?;
    let mut job_associations = context.job_associations().to_vec();
    // Carried declarations restrict roles before any mutation. Only fully
    // verified certificates become durable job associations below.
    for signed in &bundle.delegations {
        let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
        job_associations.push((body.job_public_key.clone(), body.logical_job_id.clone()));
    }
    let owners = public_owners(bundle.into(), context.now_millis() / 1000)?;
    let first = bundle
        .statements
        .first()
        .and_then(|s| s.body.as_ref())
        .ok_or(Reject::Canonical)?;
    let initial_selection = authority.for_witness(first)?;
    context.require_spool_selection(&initial_selection)?;
    require_public_selection(bundle.into(), &owners, &initial_selection)?;
    verify_complete_transfer_history(
        bundle.into(),
        &initial_selection,
        context.now_millis() / 1000,
    )?;
    // Authenticate every carried statement, including additional
    // receipts, before retaining this as a complete public proof.
    for signed in &bundle.statements {
        evidence(&bundle.history_proofs, context, signed)?;
        let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_witness(s)?;
        context.require_spool_selection(&selection)?;
        require_public_selection(bundle.into(), &owners, &selection)?;
        selection.keyring.verify_current_owner(
            selection.owner,
            s.observed_at_unix_millis / 1000,
            selection.limits,
        )?;
        if s.spool_uuid != selection.keyring.owner_genesis().spool_uuid()
            || s.spool_genesis_digest != selection.spool_genesis_digest
            || s.owner_id != selection.owner.owner_id()
            || s.owner_state_hash != selection.owner.state_hash()
            || s.ownership_transfer_sequence
                != selection.keyring.wire().ownership_transfers.len() as u64
        {
            return Err(Error::Hybrid(Reject::Root));
        }
    }
    let mut originals = bundle.original_geneses.clone();
    originals.extend_from_slice(native_records);
    for p in &bundle.authority_witnesses {
        originals.extend(p.original.iter().cloned());
        originals.extend(p.dependencies.iter().cloned());
    }
    for p in &bundle.landing_witnesses {
        originals.extend(p.execution.iter().cloned());
        originals.extend(p.source_operation.iter().cloned());
        originals.extend(p.review_evidence.iter().cloned());
    }
    let boundaries: Vec<_> = bundle
        .genesis_witnesses
        .iter()
        .filter_map(|p| p.boundary_acceptance.clone())
        .chain(
            bundle
                .authority_witnesses
                .iter()
                .flat_map(|p| p.boundary_acceptances.clone()),
        )
        .collect();
    let closure = NativeClosure::verify_with_boundaries(&originals, &boundaries)?;
    let forbidden = context.forbidden_job_keys();
    for policy in &bundle.policies {
        let p = policy.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_policy(p)?;
        context.require_spool_selection(&selection)?;
        require_public_selection(bundle.into(), &owners, &selection)?;
        selection.keyring.verify_current_owner(
            selection.owner,
            context.now_millis() / 1000,
            selection.limits,
        )?;
        if p.spool_uuid != selection.keyring.owner_genesis().spool_uuid()
            || p.owner_id != selection.owner.owner_id()
            || p.ownership_transfer_sequence
                != selection.keyring.wire().ownership_transfers.len() as u64
        {
            return Err(Error::Hybrid(Reject::Root));
        }
        permission::verify_policy_record(policy, &[selection.owner])?;
    }
    let mut delegations: BTreeMap<Vec<u8>, VerifiedImportDelegation> = BTreeMap::new();
    // Each result resolves its ORIGINAL certificate at its own exact
    // publication, never a later permission or current author timestamp.
    for operation in &bundle.operations {
        let body = operation.body.as_ref().ok_or(Reject::Canonical)?;
        let signed = bundle
            .delegations
            .iter()
            .find(|d| {
                contract::signed_delegation_digest(d).is_ok_and(|h| h == body.delegation_digest)
            })
            .ok_or(Reject::Scope)?;
        let (_, statement) = find_publication(bundle, operation)?;
        let evidence = evidence(&bundle.history_proofs, context, statement)?;
        let s = statement.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_witness(s)?;
        context.require_spool_selection(&selection)?;
        let c = current_context(selection, context, &forbidden, &job_associations);
        let member = contract::resolve_bundle_permission(
            bundle,
            &signed
                .body
                .as_ref()
                .ok_or(Reject::Canonical)?
                .parent_permission_digest,
        )?;
        let d = permission::verify_historical(
            signed,
            member,
            &c,
            statement,
            evidence.resolved(),
            context.set(),
            |r| authority.import_revoked(s, r),
        )?;
        delegations.insert(body.delegation_digest.clone(), d);
    }
    let mut admissions = BTreeMap::new();
    let mut geneses = BTreeMap::new();
    for payload in &bundle.genesis_witnesses {
        let canonical = hybrid_codec::canonical(payload)?;
        let statement = bundle
            .statements
            .iter()
            .find(|s| {
                s.body
                    .as_ref()
                    .is_some_and(|s| s.purpose == 1 && s.canonical_payload == canonical)
            })
            .ok_or(Reject::Scope)?;
        let evidence = evidence(&bundle.history_proofs, context, statement)?;
        let s = statement.body.as_ref().ok_or(Reject::Canonical)?;
        let binding = payload.binding.as_ref().ok_or(Reject::Canonical)?;
        let binding_body = binding.body.as_ref().ok_or(Reject::Canonical)?;
        let binding_digest = contract::signed_genesis_digest(binding)?;
        let signed = bundle
            .delegations
            .iter()
            .find(|d| {
                d.body.as_ref().is_some_and(|d| {
                    d.branch_manifest
                        .iter()
                        .any(|b| b.genesis_authority_digest == binding_digest)
                        && d.parent_permission_digest == binding_body.parent_permission_digest
                })
            })
            .ok_or(Reject::Scope)?;
        let selection = authority.for_witness(s)?;
        context.require_spool_selection(&selection)?;
        let c = current_context(selection, context, &forbidden, &job_associations);
        let member = contract::resolve_bundle_permission(
            bundle,
            &signed
                .body
                .as_ref()
                .ok_or(Reject::Canonical)?
                .parent_permission_digest,
        )?;
        let d = permission::verify_historical_genesis(
            signed,
            member,
            &c,
            payload,
            statement,
            evidence.resolved(),
            context.set(),
            |r| authority.import_revoked(s, r),
        )?;
        let genesis = verification::verify_genesis_payload_at_boundary(
            payload,
            &evidence,
            &d,
            &closure,
            &verification::NativeAuthorityContext {
                owner: selection.owner,
                spool_uuid: uuid::Uuid::from_bytes(selection.keyring.owner_genesis().spool_uuid()),
                spool_genesis: selection.spool_genesis_digest,
                transfer_sequence: selection.keyring.wire().ownership_transfers.len() as u64,
                spool_path: &selection
                    .keyring
                    .wire()
                    .canonical_spool_path_segments
                    .join("/"),
                witness_set: context.set(),
                original_geneses: crypto::import_authority::OriginalGeneses::Import(
                    &bundle.genesis_witnesses,
                ),
                known_job_associations: &job_associations,
                forbidden_authority_keys: &forbidden,
            },
            |r| authority.native_revoked(s, r),
        )?;
        admit_boundary_originals(
            &mut admissions,
            payload.boundary_acceptance.as_slice(),
            &originals,
            &evidence,
        )?;
        let thread = genesis.genesis().id()?;
        admissions.insert(
            thread,
            (
                payload
                    .original_genesis
                    .as_ref()
                    .ok_or(Reject::Canonical)?
                    .clone(),
                evidence,
            ),
        );
        geneses.insert(thread, genesis);
        context.retain_delegation(&d)?;
        delegations
            .entry(contract::signed_delegation_digest(signed)?)
            .or_insert(d);
    }
    for (i, renewal) in bundle.renewals.iter().enumerate() {
        let next = &bundle.delegations[i + 1];
        let previous = &bundle.delegations[i];
        let d = delegations
            .get(&contract::signed_delegation_digest(next)?)
            .ok_or(Reject::Scope)?;
        let old = delegations
            .get(&contract::signed_delegation_digest(previous)?)
            .ok_or(Reject::Scope)?;
        let r = renewal.body.as_ref().ok_or(Reject::Canonical)?;
        let manifest = contract::resolve_bundle_manifest(bundle, &r.committed_manifest_digest)?;
        // The successor has already been checked at its own exact
        // witnessed publication. Reuse that accepted owner/time context.
        let op = bundle
            .operations
            .iter()
            .find(|o| {
                o.body
                    .as_ref()
                    .is_some_and(|o| o.delegation_digest == d.scope().digest())
            })
            .ok_or(Reject::Scope)?;
        let (_, statement) = find_publication(bundle, op)?;
        let s = statement.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_witness(s)?;
        let expected = contract::ImportOwnerExpectation {
            identity: d
                .scope()
                .body()
                .identity
                .as_ref()
                .ok_or(Reject::Canonical)?,
            owner_public_key: &selection.owner.authority_key().public_key,
            owner_chain_digest: &d.scope().body().owner_chain_digest,
            authority_expires_at_seconds: i64::MAX,
            now_unix_seconds: s.observed_at_unix_millis / 1000,
            forbidden_job_keys: &forbidden,
            known_job_associations: &job_associations,
        };
        contract::verify_renewal(
            renewal,
            old.scope(),
            manifest,
            r.expected_authority_epoch,
            d.member_permission(),
            &expected,
        )?;
    }
    for statement in &bundle.statements {
        let s = statement.body.as_ref().ok_or(Reject::Canonical)?;
        let evidence = evidence(&bundle.history_proofs, context, statement)?;
        let selection = authority.for_witness(s)?;
        context.require_spool_selection(&selection)?;
        selection.keyring.verify_current_owner(
            selection.owner,
            s.observed_at_unix_millis / 1000,
            selection.limits,
        )?;
        if s.purpose == 1 {
            let payload = bundle
                .genesis_witnesses
                .iter()
                .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                .ok_or(Reject::Scope)?;
            let binding = payload.binding.as_ref().ok_or(Reject::Canonical)?;
            let digest = contract::signed_genesis_digest(binding)?;
            let parent = &binding
                .body
                .as_ref()
                .ok_or(Reject::Canonical)?
                .parent_permission_digest;
            let d = delegations
                .values()
                .find(|d| {
                    d.scope()
                        .body()
                        .branch_manifest
                        .iter()
                        .any(|b| b.genesis_authority_digest == digest)
                        && d.scope().body().parent_permission_digest == *parent
                })
                .ok_or(Reject::Scope)?;
            let c = current_context(selection, context, &forbidden, &job_associations);
            let verified = permission::verify_historical_genesis(
                d.signed(),
                d.member_permission(),
                &c,
                payload,
                statement,
                evidence.resolved(),
                context.set(),
                |r| authority.import_revoked(s, r),
            )?;
            let genesis = verification::verify_genesis_payload_at_boundary(
                payload,
                &evidence,
                &verified,
                &closure,
                &verification::NativeAuthorityContext {
                    owner: selection.owner,
                    spool_uuid: uuid::Uuid::from_bytes(
                        selection.keyring.owner_genesis().spool_uuid(),
                    ),
                    spool_genesis: selection.spool_genesis_digest,
                    transfer_sequence: selection.keyring.wire().ownership_transfers.len() as u64,
                    spool_path: &selection
                        .keyring
                        .wire()
                        .canonical_spool_path_segments
                        .join("/"),
                    witness_set: context.set(),
                    original_geneses: crypto::import_authority::OriginalGeneses::Import(
                        &bundle.genesis_witnesses,
                    ),
                    known_job_associations: &job_associations,
                    forbidden_authority_keys: &forbidden,
                },
                |r| authority.native_revoked(s, r),
            )?;
            admit_boundary_originals(
                &mut admissions,
                payload.boundary_acceptance.as_slice(),
                &originals,
                &evidence,
            )?;
            admissions.insert(
                genesis.genesis().id()?,
                (
                    payload
                        .original_genesis
                        .as_ref()
                        .ok_or(Reject::Canonical)?
                        .clone(),
                    evidence,
                ),
            );
            continue;
        }
        if s.purpose == 3 {
            let (operation, manifest) = bundle
                .operations
                .iter()
                .find_map(|operation| {
                    bundle
                        .manifests
                        .iter()
                        .find(|manifest| {
                            contract::publication_payload(operation, manifest)
                                .and_then(|p| hybrid_codec::canonical(&p))
                                .is_ok_and(|p| p == s.canonical_payload)
                        })
                        .map(|manifest| (operation, manifest))
                })
                .ok_or(Reject::Scope)?;
            let body = operation.body.as_ref().ok_or(Reject::Canonical)?;
            let d = delegations
                .get(&body.delegation_digest)
                .ok_or(Reject::Scope)?;
            let c = current_context(selection, context, &forbidden, &job_associations);
            permission::verify_historical(
                d.signed(),
                d.member_permission(),
                &c,
                statement,
                evidence.resolved(),
                context.set(),
                |r| authority.import_revoked(s, r),
            )?;
            contract::verify_publication(
                operation,
                d.scope(),
                manifest,
                statement,
                context.set(),
                evidence.proof(),
                context.now_millis(),
            )?;
            continue;
        }
        let spool = uuid::Uuid::from_bytes(selection.keyring.owner_genesis().spool_uuid());
        let path = selection
            .keyring
            .wire()
            .canonical_spool_path_segments
            .join("/");
        let native = verification::NativeAuthorityContext {
            owner: selection.owner,
            spool_uuid: spool,
            spool_genesis: selection.spool_genesis_digest,
            transfer_sequence: selection.keyring.wire().ownership_transfers.len() as u64,
            spool_path: &path,
            witness_set: context.set(),
            original_geneses: crypto::import_authority::OriginalGeneses::Import(
                &bundle.genesis_witnesses,
            ),
            known_job_associations: &job_associations,
            forbidden_authority_keys: &forbidden,
        };
        if s.purpose == 2 {
            let p = bundle
                .authority_witnesses
                .iter()
                .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                .ok_or(Reject::Scope)?;
            verification::verify_authority_payload(p, &evidence, &closure, &native, |r| {
                authority.native_revoked(s, r)
            })?;
            admit_boundary_originals(
                &mut admissions,
                &p.boundary_acceptances,
                &originals,
                &evidence,
            )?;
            let record = p.original.as_ref().ok_or(Reject::Canonical)?;
            admissions.insert(native_subject(record)?.0, (record.clone(), evidence));
        } else {
            let p = bundle
                .landing_witnesses
                .iter()
                .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                .ok_or(Reject::Scope)?;
            verification::verify_landing_payload(p, &evidence, &closure, &native, |r| {
                authority.native_revoked(s, r)
            })?;
            let record = p.execution.as_ref().ok_or(Reject::Canonical)?;
            admissions.insert(native_subject(record)?.0, (record.clone(), evidence));
        }
    }
    // Index already verified originals once. Complete public histories can
    // carry many slots with no requested converted counterpart.
    let mut native_frontiers = BTreeMap::new();
    for record in originals
        .iter()
        .filter(|record| record.format == objects::object::thread_replication::OPERATION_FORMAT)
    {
        let id = ContentHash::compute_typed(&record.format, &record.canonical_record);
        let op = closure.operation(&id)?;
        let frontier = contract::frontier_digest(&wire::ImportFrontierV1 {
            format_version: 1,
            thread_id: op.thread.as_bytes().to_vec(),
            operation_ids: vec![id.as_bytes().to_vec()],
        })?;
        native_frontiers.insert(frontier, record);
    }
    for operation in &bundle.operations {
        let body = operation.body.as_ref().ok_or(Reject::Canonical)?;
        let d = delegations
            .get(&body.delegation_digest)
            .ok_or(Reject::Scope)?;
        let thread = ContentHash::from_bytes(
            body.target_thread_id
                .as_slice()
                .try_into()
                .map_err(|_| Reject::Canonical)?,
        );
        let original = geneses.get(&thread).ok_or(Reject::Scope)?;
        // Public history authenticates all publications and slots, even when
        // this carrier does not request their converted native counterparts.
        context.retain_delegation(d)?;
        context.retain_slot(operation)?;
        let Some(converted) = native_frontiers.get(&body.resulting_frontier_digest) else {
            continue;
        };
        let converted = *converted;
        let (_, native_operation) = verification::verify_native_operation(converted)?;
        let converter = delegations
            .values()
            .find(|v| {
                v.scope().body().job_public_key == native_operation.publisher
                    && v.scope().body().logical_job_id == d.scope().body().logical_job_id
                    && v.scope().body().retry_lineage_id == d.scope().body().retry_lineage_id
            })
            .ok_or(Reject::KeyRole)?;
        let parents = native_operation
            .parents
            .iter()
            .map(|id| closure.operation(id).cloned())
            .collect::<verification::Result<Vec<ThreadOperation>>>()?;
        let content = verification::verify_delegated_import(
            operation, d, converter, original, converted, &parents,
        )?;
        let (manifest, statement) = find_publication(bundle, operation)?;
        let evidence = evidence(&bundle.history_proofs, context, statement)?;
        verification::verify_publication(
            &content,
            d,
            manifest,
            &evidence,
            context.set(),
            context.now_millis(),
        )?;
        admissions.insert(native_operation.id()?, (converted.clone(), evidence));
    }
    let selected = selected_originals(native_records, &originals)?;
    // Signature verification is not admission. Match exact originals, including
    // their signatures, before any receives or filesystem callback can run.
    for (id, record) in &selected {
        if !admissions
            .get(id)
            .is_some_and(|(admitted, _)| admitted == record)
        {
            return Err(Error::Hybrid(Reject::ImportPermission));
        }
    }
    let geneses = geneses
        .into_iter()
        .map(|(id, g)| {
            (
                id,
                (
                    g.original().clone(),
                    g.payload().creator_authority_envelope.clone(),
                ),
            )
        })
        .collect();
    install_selected_in(
        directory,
        selected,
        &geneses,
        &admissions,
        &closure,
        store,
        context,
        |id| retain_bundle(context, id, bundle),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_selected_in(
    directory: &Path,
    selected: BTreeMap<ContentHash, wire::SignedRecord>,
    geneses: &BTreeMap<ContentHash, (crypto::thread_operation::SignedGenesis, Vec<u8>)>,
    admissions: &BTreeMap<ContentHash, (wire::SignedRecord, WitnessEvidence)>,
    closure: &NativeClosure,
    store: &impl ObjectStore,
    context: &TrustTransaction<'_>,
    retain: impl Fn(&ContentHash) -> Result<()>,
) -> Result<Vec<ThreadReplica>> {
    let mut replicas = BTreeMap::new();
    for (id, record) in &selected {
        if record.format != objects::object::thread_replication::GENESIS_FORMAT {
            continue;
        }
        let genesis = geneses.get(id).ok_or(Reject::Scope)?;
        let replica = ThreadReplica {
            path: directory.join(crate::local_metadata::DATABASE_NAME),
            thread: *id,
        };
        replica.create_with_proof_in(context.sql(), &genesis.0, &genesis.1, None)?;
        retain(id)?;
        replicas.insert(*id, replica);
    }
    // Dependency order comes from the signed native DAG, never carrier order.
    // Cache edges once; a long selected chain must not repeatedly decode or
    // authenticate every still-pending original.
    // Co-signed claims freeze prior local work. Admit that checked historical
    // work before installing any competing cutoff on the same Thread.
    let mut local_work: BTreeMap<ContentHash, Vec<ContentHash>> = BTreeMap::new();
    for (id, record) in &selected {
        if record.format == objects::object::thread_replication::OPERATION_FORMAT {
            let op = verification::verify_native_operation(record)?.1;
            if matches!(
                op.source_author()?,
                Some(objects::object::thread_replication::SourceAuthor::LocalKey)
            ) {
                local_work.entry(op.thread).or_default().push(*id);
            }
        }
    }
    let mut ready = BTreeSet::new();
    let mut remaining = BTreeMap::new();
    let mut dependents: BTreeMap<ContentHash, Vec<ContentHash>> = BTreeMap::new();
    for (id, record) in &selected {
        let (_, thread, dependencies) = native_subject(record)?;
        let mut dependencies: BTreeSet<_> = dependencies.into_iter().collect();
        if record.format == objects::object::thread_replication::ownership_claim::FORMAT {
            dependencies.extend(local_work.get(&thread).into_iter().flatten().copied());
        }
        if dependencies.is_empty() {
            ready.insert(*id);
        }
        remaining.insert(*id, (thread, dependencies.len()));
        for dependency in dependencies {
            dependents.entry(dependency).or_default().push(*id);
        }
    }
    let mut pending = selected;
    while let Some(id) = ready.pop_first() {
        let (thread, _) = remaining.remove(&id).ok_or(Reject::Scope)?;
        let record = pending.remove(&id).ok_or(Reject::Scope)?;
        let replica = replicas.get(&thread).ok_or(Reject::Scope)?;
        if let Some((_, evidence)) = admissions.get(&id) {
            retain_statement(context, id, evidence)?;
        }
        match record.format.as_str() {
            objects::object::thread_replication::GENESIS_FORMAT => {}
            objects::object::thread_replication::OPERATION_FORMAT => {
                let (signed, operation) = verification::verify_native_operation(&record)?;
                replica.validate_reference_capture_in(
                    context.sql(),
                    closure.genesis(&thread)?,
                    &operation,
                    store,
                )?;
                if replica.receive_verified_in(
                    context.sql(),
                    &signed,
                    &operation,
                    store,
                    false,
                    None,
                    false,
                )? != Admission::Accepted
                {
                    return Err(Error::Hybrid(Reject::Scope));
                }
            }
            objects::object::thread_replication::ownership_claim::FORMAT => {
                replica.install_verified_claim_in(context.sql(), &record)?;
            }
            objects::object::thread_replication::ownership_resolution::FORMAT => {
                replica.install_verified_resolution_in(context.sql(), &record)?;
            }
            _ => return Err(Error::Hybrid(Reject::Version)),
        }
        if let Some(children) = dependents.remove(&id) {
            for child in children {
                let (_, count) = remaining.get_mut(&child).ok_or(Reject::Scope)?;
                *count = count.checked_sub(1).ok_or(Reject::Scope)?;
                if *count == 0 {
                    ready.insert(child);
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(Error::Hybrid(Reject::Scope));
    }
    Ok(replicas.into_values().collect())
}

// Called only after the native payload verifier authenticates the complete
// boundary selection, each original receipt and the accepting authority.
// Each selected dependency still needs its own exact verified receipt.
pub(super) fn admit_boundary_originals(
    admissions: &mut BTreeMap<ContentHash, (wire::SignedRecord, WitnessEvidence)>,
    boundaries: &[wire::ImportBoundaryAcceptanceV1],
    originals: &[wire::SignedRecord],
    evidence: &WitnessEvidence,
) -> Result<()> {
    use objects::object::{
        thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission},
        thread_genesis_admission::ThreadGenesisAdmission,
    };
    for receipt in boundaries
        .iter()
        .flat_map(|boundary| &boundary.original_receipts)
    {
        let (id, format) = match receipt.format.as_str() {
            "heddle-thread-genesis-admission-v2" => (
                ThreadGenesisAdmission::decode(&receipt.canonical_record)?.thread,
                objects::object::thread_replication::GENESIS_FORMAT,
            ),
            "heddle-thread-authority-admission-v3" => {
                match ThreadAuthorityAdmission::decode(&receipt.canonical_record)?.subject {
                    OriginalAuthoritySubject::Operation(id) => {
                        (id, objects::object::thread_replication::OPERATION_FORMAT)
                    }
                    OriginalAuthoritySubject::OwnershipClaim(id) => (
                        id,
                        objects::object::thread_replication::ownership_claim::FORMAT,
                    ),
                    OriginalAuthoritySubject::OwnershipResolution(id) => (
                        id,
                        objects::object::thread_replication::ownership_resolution::FORMAT,
                    ),
                }
            }
            _ => return Err(Error::Hybrid(Reject::BoundaryAcceptance)),
        };
        let original = originals
            .iter()
            .filter(|record| record.format == format)
            .find(|record| native_subject(record).is_ok_and(|subject| subject.0 == id))
            .ok_or(Reject::BoundaryAcceptance)?;
        admissions
            .entry(id)
            .or_insert_with(|| (original.clone(), evidence.clone()));
    }
    Ok(())
}

/// IDs and required native dependencies; signatures are checked by NativeClosure.
pub(super) fn native_subject(
    record: &wire::SignedRecord,
) -> Result<(ContentHash, ContentHash, Vec<ContentHash>)> {
    use objects::object::thread_replication::{self as native, ThreadOperationBody};
    let (id, thread, mut dependencies) = match record.format.as_str() {
        native::GENESIS_FORMAT => {
            let genesis = native::ThreadGenesis::decode(&record.canonical_record)?;
            (genesis.id()?, genesis.id()?, Vec::new())
        }
        native::OPERATION_FORMAT => {
            let operation = native::ThreadOperation::decode(&record.canonical_record)?;
            let mut dependencies = operation.parents.iter().copied().collect::<Vec<_>>();
            if let ThreadOperationBody::Integration(bytes) = &operation.body {
                let landing = native::integration::HostedIntegration::decode(bytes)?;
                dependencies.push(landing.source_operation);
                dependencies.extend(landing.review_evidence);
            }
            if let Some(integration) = operation.local_integration()? {
                dependencies.push(integration.source_operation);
            }
            (operation.id()?, operation.thread, dependencies)
        }
        native::ownership_claim::FORMAT => {
            let claim =
                native::ownership_claim::ThreadOwnershipClaim::decode(&record.canonical_record)?;
            (
                claim.id()?,
                claim.thread,
                claim.source_frontier.into_iter().collect(),
            )
        }
        native::ownership_resolution::FORMAT => {
            let resolution = native::ownership_resolution::ThreadOwnershipResolution::decode(
                &record.canonical_record,
            )?;
            let dependencies = resolution
                .conflicting_claims
                .iter()
                .copied()
                .chain(resolution.frontier.iter().copied())
                .collect();
            (resolution.id()?, resolution.thread, dependencies)
        }
        _ => return Err(Error::Hybrid(Reject::Version)),
    };
    if id != thread {
        dependencies.push(thread);
    }
    Ok((id, thread, dependencies))
}

pub(super) fn selected_originals(
    requested: &[wire::SignedRecord],
    originals: &[wire::SignedRecord],
) -> Result<BTreeMap<ContentHash, wire::SignedRecord>> {
    let mut available = BTreeMap::new();
    for record in originals {
        // Boundary receipts are public evidence, rather than native subjects.
        if matches!(
            record.format.as_str(),
            "heddle-original-boundary-acceptance-v1"
                | "heddle-thread-genesis-admission-v2"
                | "heddle-thread-authority-admission-v3"
        ) {
            continue;
        }
        let (id, _, _) = native_subject(record)?;
        if available
            .insert(id, record)
            .is_some_and(|previous| previous != record)
        {
            return Err(Error::Hybrid(Reject::Canonical));
        }
    }
    let mut pending = requested.to_vec();
    let mut selected = BTreeMap::new();
    while let Some(record) = pending.pop() {
        let (id, _, dependencies) = native_subject(&record)?;
        if let Some(previous) = selected.insert(id, record.clone()) {
            if previous != record {
                return Err(Error::Hybrid(Reject::Canonical));
            }
            continue;
        }
        for dependency in dependencies {
            pending.push((*available.get(&dependency).ok_or(Reject::Scope)?).clone());
        }
    }
    Ok(selected)
}

fn replica_genesis(
    context: &TrustTransaction<'_>,
    thread: ContentHash,
) -> Result<crypto::thread_operation::SignedGenesis> {
    Ok(context.sql().query_row(
        "SELECT genesis,genesis_signature FROM threads WHERE id=?1",
        [thread.as_bytes()],
        |r| {
            Ok(crypto::thread_operation::SignedGenesis {
                canonical: r.get(0)?,
                signature: r.get(1)?,
            })
        },
    )?)
}
fn retain_bundle(
    context: &TrustTransaction<'_>,
    thread: &ContentHash,
    bundle: &wire::ImportPublicProofBundleV1,
) -> Result<()> {
    let old: Option<Vec<u8>> = context
        .sql()
        .query_row(
            "SELECT bundle FROM hosted_import_proofs WHERE thread=?1",
            [thread.as_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(bytes) = old {
        let old: wire::ImportPublicProofBundleV1 =
            hybrid_codec::strict_decode(&bytes, contract::MAX_BUNDLE_BYTES)?;
        if old.owner_genesis != bundle.owner_genesis
            || !bundle
                .ownership_transfers
                .starts_with(&old.ownership_transfers)
            || old
                .original_geneses
                .iter()
                .any(|o| !bundle.original_geneses.contains(o))
            || old
                .creator_authority_envelopes
                .iter()
                .any(|e| !bundle.creator_authority_envelopes.contains(e))
            || old
                .genesis_authorities
                .iter()
                .any(|g| !bundle.genesis_authorities.contains(g))
            || old
                .statements
                .iter()
                .any(|s| !bundle.statements.contains(s))
            || old
                .operations
                .iter()
                .any(|o| !bundle.operations.contains(o))
            || old
                .delegations
                .iter()
                .any(|d| !bundle.delegations.contains(d))
            || old
                .member_permissions
                .iter()
                .any(|p| !bundle.member_permissions.contains(p))
            || old.manifests.iter().any(|m| !bundle.manifests.contains(m))
            || old.renewals.iter().any(|r| !bundle.renewals.contains(r))
            || old.policies.iter().any(|p| !bundle.policies.contains(p))
            || old
                .owner_histories
                .iter()
                .any(|h| !bundle.owner_histories.contains(h))
            || old
                .authority_witnesses
                .iter()
                .any(|w| !bundle.authority_witnesses.contains(w))
            || old
                .landing_witnesses
                .iter()
                .any(|w| !bundle.landing_witnesses.contains(w))
        {
            return Err(Error::Hybrid(Reject::SlotConflict));
        }
    }
    context.sql().execute("INSERT INTO hosted_import_proofs(thread,authority,bundle) VALUES(?1,?2,?3) ON CONFLICT(thread) DO UPDATE SET bundle=excluded.bundle",params![thread.as_bytes(),context.set().body().deployment_authority,bundle.encode_to_vec()])?;
    Ok(())
}

/// Native first-admission or landing subject, retaining exact API originals.
pub enum NativeSubject<'a> {
    Authority(&'a wire::ImportAuthorityWitnessV1),
    Landing(&'a wire::HostedLandingWitnessV1),
}
/// Complete evidence for one already-known Thread. Dependency objects must be
/// separately admitted under their own original-author proofs before install.
pub struct NativeEvidence<'a> {
    pub set: &'a host::SignedHostedWitnessSetV1,
    pub statement: &'a host::SignedHostedWitnessStatementV1,
    pub proof: Option<&'a host::HostedWitnessHistoryProofV1>,
    pub policy: &'a wire::SignedSpoolPolicyRecord,
    pub originals: &'a [wire::SignedRecord],
    pub genesis_witnesses: &'a [wire::ImportGenesisWitnessV1],
    pub subject: NativeSubject<'a>,
}
impl ThreadReplica {
    /// Part 2's native source/control/landing receive seam. Resolve trust and
    /// accepted original owner/policy inside the mutation transaction, then
    /// check current disclosure with `authorize`. Job grants cannot authorize
    /// any native control or landing; pending dependency closure fails closed.
    pub fn receive_witnessed(
        &self,
        trust: &HostedTrust<impl Clock>,
        input: &NativeEvidence<'_>,
        authority: &impl AcceptedAuthority,
        store: &impl ObjectStore,
        authorize: impl Fn(&ThreadOperation, i64) -> Result<()>,
    ) -> Result<Admission> {
        if self.path.parent().ok_or(Reject::Root)?.canonicalize()?
            != trust.directory().canonicalize()?
        {
            return Err(Error::Hybrid(Reject::Root));
        }
        let final_original = match input.subject {
            NativeSubject::Authority(p) => p.original.as_ref(),
            NativeSubject::Landing(p) => p.execution.as_ref(),
        }
        .ok_or(Reject::Canonical)?;
        let (_, final_operation) = verification::verify_native_operation(final_original)?;
        let admitted = trust.mutate_validated(
            input.set,
            |context| {
                let evidence = WitnessEvidence::resolve(
                    context.set(),
                    input.statement,
                    input.proof,
                    false,
                    context.now_millis(),
                )?;
                let statement = input.statement.body.as_ref().ok_or(Reject::Canonical)?;
                let selection = authority.for_witness(statement)?;
                context.require_spool_selection(&selection)?;
                selection.keyring.verify_current_owner(
                    selection.owner,
                    statement.observed_at_unix_millis / 1000,
                    selection.limits,
                )?;
                let policy = input.policy.body.as_ref().ok_or(Reject::Canonical)?;
                if policy.spool_uuid != statement.spool_uuid
                    || policy.owner_id != statement.owner_id
                    || policy.owner_state_hash != statement.owner_state_hash
                    || policy.ownership_transfer_sequence != statement.ownership_transfer_sequence
                    || policy.sequence != statement.policy_sequence
                    || policy.policy_state_hash != statement.policy_state_hash
                {
                    return Err(Error::Hybrid(Reject::Scope));
                }
                permission::verify_policy_record(input.policy, &[selection.owner])?;
                let boundaries = match input.subject {
                    NativeSubject::Authority(p) => p.boundary_acceptances.as_slice(),
                    NativeSubject::Landing(_) => &[],
                };
                let closure = NativeClosure::verify_with_boundaries(input.originals, boundaries)?;
                let path = selection
                    .keyring
                    .wire()
                    .canonical_spool_path_segments
                    .join("/");
                let native = verification::NativeAuthorityContext {
                    owner: selection.owner,
                    spool_uuid: uuid::Uuid::from_bytes(
                        selection.keyring.owner_genesis().spool_uuid(),
                    ),
                    spool_genesis: selection.spool_genesis_digest,
                    transfer_sequence: selection.keyring.wire().ownership_transfers.len() as u64,
                    spool_path: &path,
                    witness_set: context.set(),
                    original_geneses: verification::OriginalGeneses::Import(
                        input.genesis_witnesses,
                    ),
                    known_job_associations: context.job_associations(),
                    forbidden_authority_keys: &context.forbidden_job_keys(),
                };
                let original = match input.subject {
                    NativeSubject::Authority(payload) => {
                        verification::verify_authority_payload(
                            payload,
                            &evidence,
                            &closure,
                            &native,
                            |r| authority.native_revoked(statement, r),
                        )?;
                        if payload.kind != 1 {
                            return Err(Error::Hybrid(Reject::ImportPermission));
                        }
                        payload.original.as_ref().ok_or(Reject::Canonical)?
                    }
                    NativeSubject::Landing(payload) => {
                        verification::verify_landing_payload(
                            payload,
                            &evidence,
                            &closure,
                            &native,
                            |r| authority.native_revoked(statement, r),
                        )?;
                        payload.execution.as_ref().ok_or(Reject::Canonical)?
                    }
                };
                let (signed, operation) = verification::verify_native_operation(original)?;
                if operation.thread != self.thread
                    || context
                        .job_associations()
                        .iter()
                        .any(|(key, _)| key == &operation.publisher)
                {
                    return Err(Error::Hybrid(Reject::KeyRole));
                }
                let genesis = replica_genesis(context, self.thread)?.verify()?;
                if closure.genesis(&self.thread)? != &genesis {
                    return Err(Error::Hybrid(Reject::Root));
                }
                authorize(&operation, context.now_millis())?;
                self.validate_reference_capture(&operation, store)?;
                retain_statement(context, operation.id()?, &evidence)?;
                let admitted = self.receive_verified_in(
                    context.sql(),
                    &signed,
                    &operation,
                    store,
                    false,
                    None,
                    false,
                )?;
                if admitted != Admission::Accepted {
                    return Err(Error::Hybrid(Reject::Scope));
                }
                Ok(admitted)
            },
            |_, now| authorize(&final_operation, now),
        )?;
        self.notify_committed()?;
        Ok(admitted)
    }
}
pub(super) fn retain_statement(
    context: &TrustTransaction<'_>,
    id: ContentHash,
    evidence: &WitnessEvidence,
) -> Result<()> {
    let bytes = evidence.signed().encode_to_vec();
    let stored: Option<Vec<u8>> = context
        .sql()
        .query_row(
            "SELECT statement FROM hosted_import_admissions WHERE operation=?1",
            [id.as_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    if stored.as_ref().is_some_and(|old| old != &bytes) {
        return Err(Error::Hybrid(Reject::SlotConflict));
    }
    context.sql().execute("INSERT INTO hosted_import_admissions(operation,authority,statement,proof) VALUES(?1,?2,?3,?4) ON CONFLICT(operation) DO UPDATE SET proof=excluded.proof",params![id.as_bytes(),context.set().body().deployment_authority,bytes,evidence.proof().map(Message::encode_to_vec)])?;
    Ok(())
}
