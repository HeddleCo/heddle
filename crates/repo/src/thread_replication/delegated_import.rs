//! Part 2's verify-before-install seam for complete HYBRID public evidence.
//! Transport authenticates disclosure/delivery separately. This module retains
//! unchanged originals, resolves each witness under receiver-owned trust, and
//! commits genesis/content/proof/job associations in the same transaction.
use std::{collections::BTreeMap, path::Path};

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
    ) -> Result<()>;
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

fn evidence(
    bundle: &wire::ImportPublicProofBundleV1,
    context: &TrustTransaction<'_>,
    signed: &host::SignedHostedWitnessStatementV1,
) -> Result<WitnessEvidence> {
    match WitnessEvidence::resolve(context.set(), signed, None, false, context.now_millis()) {
        Ok(e) => Ok(e),
        Err(verification::Error::Contract(Reject::Proof)) => {
            let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
            for proof in &bundle.history_proofs {
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
) -> CurrentContext<'a> {
    CurrentContext {
        selection,
        now: context.now_millis() / 1000,
        forbidden_job_keys: forbidden,
        known_job_associations: context.job_associations(),
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

fn public_owners(
    bundle: &wire::ImportPublicProofBundleV1,
    now: i64,
) -> Result<BTreeMap<[u8; 32], heddleco_capability_verifier::VerifiedOwnerState>> {
    let mut states = BTreeMap::new();
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600)?;
    for history in &bundle.owner_histories {
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
fn require_public_selection(
    bundle: &wire::ImportPublicProofBundleV1,
    states: &BTreeMap<[u8; 32], heddleco_capability_verifier::VerifiedOwnerState>,
    selection: &Selection<'_>,
) -> Result<()> {
    if bundle.owner_genesis.as_ref() != Some(selection.keyring.owner_genesis().signed())
        || bundle.ownership_transfers != selection.keyring.wire().ownership_transfers
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

    /// Install a complete bounded import export, including post-renewal history.
    /// Canonical protobuf is checked before trusting typed fields. Native closure
    /// objects may be staged in the object store first; no checkout is changed.
    /// A fresh set and receiver clock are mandatory even for exact replay.
    pub fn install_hybrid_import(
        directory: &Path,
        trust: &HostedTrust<impl Clock>,
        bundle_bytes: &[u8],
        native_records: &[wire::SignedRecord],
        authority: &impl AcceptedAuthority,
        store: &impl ObjectStore,
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
        let replicas = trust.mutate(signed_set, |context| {
            install_in(
                directory,
                &bundle,
                native_records,
                authority,
                store,
                context,
            )
        })?;
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
    authority.authorize_import(bundle, context.now_millis())?;
    let owners = public_owners(bundle, context.now_millis() / 1000)?;
    // Authenticate every carried statement, including additional
    // receipts, before retaining this as a complete public proof.
    for signed in &bundle.statements {
        evidence(bundle, context, signed)?;
        let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_witness(s)?;
        context.require_spool_selection(&selection)?;
        require_public_selection(bundle, &owners, &selection)?;
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
    let closure = NativeClosure::verify(&originals)?;
    let forbidden = context.forbidden_job_keys();
    for policy in &bundle.policies {
        let p = policy.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_policy(p)?;
        context.require_spool_selection(&selection)?;
        require_public_selection(bundle, &owners, &selection)?;
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
        let evidence = evidence(bundle, context, statement)?;
        let s = statement.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_witness(s)?;
        context.require_spool_selection(&selection)?;
        let c = current_context(selection, context, &forbidden);
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
    let mut replicas = BTreeMap::new();
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
        let evidence = evidence(bundle, context, statement)?;
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
        let c = current_context(selection, context, &forbidden);
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
        let genesis = verification::verify_genesis_payload(payload, &evidence, &d, |r| {
            authority.native_revoked(s, r)
        })?;
        retain_statement(context, genesis.genesis().id()?, &evidence)?;
        let replica = ThreadReplica {
            path: directory.join(crate::local_metadata::DATABASE_NAME),
            thread: genesis.genesis().id()?,
        };
        replica.create_with_proof_in(
            context.sql(),
            genesis.original(),
            &payload.creator_authority_envelope,
            None,
        )?;
        retain_bundle(context, &replica.thread, bundle)?;
        geneses.insert(replica.thread, genesis);
        replicas.insert(replica.thread, replica);
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
            known_job_associations: context.job_associations(),
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
        let evidence = evidence(bundle, context, statement)?;
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
            let c = current_context(selection, context, &forbidden);
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
            let genesis =
                verification::verify_genesis_payload(payload, &evidence, &verified, |r| {
                    authority.native_revoked(s, r)
                })?;
            retain_statement(context, genesis.genesis().id()?, &evidence)?;
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
            let c = current_context(selection, context, &forbidden);
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
        } else {
            let p = bundle
                .landing_witnesses
                .iter()
                .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                .ok_or(Reject::Scope)?;
            verification::verify_landing_payload(p, &evidence, &closure, &native, |r| {
                authority.native_revoked(s, r)
            })?;
        }
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
        let replica = replicas.get(&thread).ok_or(Reject::Scope)?;
        let original = geneses.get(&thread).ok_or(Reject::Scope)?;
        let mut converted = None;
        for record in native_records {
            let (_, op) = verification::verify_native_operation(record)?;
            if op.thread == thread
                && contract::frontier_digest(&wire::ImportFrontierV1 {
                    format_version: 1,
                    thread_id: thread.as_bytes().to_vec(),
                    operation_ids: vec![op.id()?.as_bytes().to_vec()],
                })? == body.resulting_frontier_digest
            {
                converted = Some(record);
                break;
            }
        }
        let converted = converted.ok_or(Reject::Scope)?;
        let (native_signed, native_operation) = verification::verify_native_operation(converted)?;
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
        let evidence = evidence(bundle, context, statement)?;
        verification::verify_publication(
            &content,
            d,
            manifest,
            &evidence,
            context.set(),
            context.now_millis(),
        )?;
        context.retain_delegation(d)?;
        context.retain_slot(operation)?;
        retain_statement(context, native_operation.id()?, &evidence)?;
        if replica.receive_verified_in(
            context.sql(),
            &native_signed,
            &native_operation,
            store,
            false,
            None,
            false,
        )? != Admission::Accepted
        {
            return Err(Error::Hybrid(Reject::Scope));
        }
    }
    authority.authorize_import(bundle, context.now_millis())?;
    Ok(replicas.into_values().collect::<Vec<_>>())
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
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        if self.path.parent().ok_or(Reject::Root)?.canonicalize()?
            != trust.directory().canonicalize()?
        {
            return Err(Error::Hybrid(Reject::Root));
        }
        let admitted = trust.mutate(input.set, |context| {
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
            let closure = NativeClosure::verify(input.originals)?;
            let path = selection
                .keyring
                .wire()
                .canonical_spool_path_segments
                .join("/");
            let native = verification::NativeAuthorityContext {
                owner: selection.owner,
                spool_uuid: uuid::Uuid::from_bytes(selection.keyring.owner_genesis().spool_uuid()),
                spool_genesis: selection.spool_genesis_digest,
                transfer_sequence: selection.keyring.wire().ownership_transfers.len() as u64,
                spool_path: &path,
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
            authorize(&operation)?;
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
        })?;
        self.notify_committed()?;
        Ok(admitted)
    }
}
fn retain_statement(
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
