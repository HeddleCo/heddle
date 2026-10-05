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
pub(super) fn find_publication<'a>(
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
    /// Public evidence includes the complete cumulative publication history.
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
                    None,
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
struct ImportOwnerFacts {
    identity: wire::ImportIdentityV1,
    chain: Vec<u8>,
    expiry: i64,
    key: Vec<u8>,
    forbidden: Vec<Vec<u8>>,
    landing_forbidden: Vec<Vec<u8>>,
    from: i64,
    until: Option<i64>,
}
fn owner_fact_at<'a>(
    timeline: &'a [ImportOwnerFacts],
    identity: &wire::ImportIdentityV1,
    time: Option<i64>,
) -> std::result::Result<&'a ImportOwnerFacts, Reject> {
    timeline
        .iter()
        .rev()
        .find(|fact| match time {
            Some(t) => t >= fact.from && fact.until.is_none_or(|end| t < end),
            None => fact.identity == *identity,
        })
        .ok_or(Reject::Scope)
}

#[cfg(test)]
#[path = "import_owner_interval_tests.rs"]
mod owner_interval_tests;
impl ImportOwnerFacts {
    fn expectation<'a>(
        &'a self,
        associations: &'a [(Vec<u8>, Vec<u8>)],
    ) -> contract::ImportBundleOwnerExpectation<'a> {
        contract::ImportBundleOwnerExpectation {
            identity: &self.identity,
            owner_public_key: &self.key,
            owner_chain_digest: &self.chain,
            authority_expires_at_seconds: self.expiry,
            effective_from_unix_seconds: self.from,
            effective_until_unix_seconds: self.until,
            forbidden_job_keys: &self.forbidden,
            forbidden_landing_keys: &self.landing_forbidden,
            known_job_associations: associations,
        }
    }
}
// Public states were replayed and authenticated against the independently selected
// root. Use the next accepted state as the end, including claims clearing deferral.
fn import_owner_facts(
    states: &BTreeMap<[u8; 32], heddleco_capability_verifier::VerifiedOwnerState>,
    selection: Selection<'_>,
    forbidden: Vec<Vec<u8>>,
    transfers: &[wire::ResourceTransferAuditRecord],
) -> Result<Vec<ImportOwnerFacts>> {
    let mut timeline = states
        .values()
        .filter(|s| s.signed_root() == selection.owner.signed_root())
        .collect::<Vec<_>>();
    timeline.sort_by_key(|s| s.sequence());
    if timeline
        .windows(2)
        .any(|w| w[0].sequence() == w[1].sequence())
    {
        return Err(Reject::Root.into());
    }
    let account = &selection
        .owner
        .signed_root()
        .root
        .as_ref()
        .ok_or(Reject::Root)?
        .account_uuid;
    let mut transfer_from = 0;
    let mut transfer_until = None;
    for record in transfers {
        let handoff = record
            .transfer
            .as_ref()
            .and_then(|t| t.acceptance.as_ref())
            .and_then(|a| a.signed_handoff.as_ref())
            .and_then(|h| h.handoff.as_ref())
            .ok_or(Reject::Root)?;
        if handoff.destination_owner_uuid == *account {
            transfer_from = record.committed_at_unix_seconds;
        }
        if handoff.source_owner_uuid == *account {
            transfer_until = Some(record.committed_at_unix_seconds);
            break;
        }
    }
    timeline
        .iter()
        .enumerate()
        .map(|(i, owner)| {
            let (identity, chain, expiry) =
                permission::native_lineage(&Selection { owner, ..selection })?;
            let landing_forbidden = forbidden.clone();
            let mut forbidden = forbidden.clone();
            forbidden.extend(owner.authority_public_keys());
            forbidden.extend(selection.keyring.authority_public_keys().cloned());
            Ok(ImportOwnerFacts {
                identity,
                chain,
                expiry,
                key: owner.authority_key().public_key.clone(),
                landing_forbidden,
                forbidden,
                from: owner.valid_from_unix_seconds().max(transfer_from),
                until: match (
                    timeline
                        .get(i + 1)
                        .map(|next| next.valid_from_unix_seconds()),
                    transfer_until,
                ) {
                    (Some(next), Some(transfer)) => Some(next.min(transfer)),
                    (next, transfer) => next.or(transfer),
                },
            })
        })
        .collect()
}

/// Compose API-owned admission order, single-delegation cumulative
/// budgets with independently verified owner/policy/native contexts.
fn verify_import_history(
    bundle: &wire::ImportPublicProofBundleV1,
    authority: &impl AcceptedAuthority,
    context: &TrustTransaction<'_>,
    recheck: bool,
) -> Result<()> {
    let snapshot = if recheck {
        None
    } else {
        context.import_witness_snapshot()?
    };
    verify_witnessed_import_bundle(
        bundle,
        authority,
        &context.import_witness_pin(),
        snapshot.as_ref(),
        context.now_millis(),
        context.job_associations(),
        &context.forbidden_job_keys(),
        |selection| context.require_spool_selection(selection),
    )?;
    if let Some(snapshot) = snapshot.as_ref() {
        let current = bundle.encode_to_vec();
        for old in import_job_history(snapshot, bundle) {
            context.sql().execute(
                "UPDATE hosted_import_proofs SET bundle=?3 WHERE authority=?1 AND bundle=?2",
                params![
                    context.set().body().deployment_authority,
                    old.encode_to_vec(),
                    current
                ],
            )?;
        }
    }
    Ok(())
}

fn import_job_history<'a>(
    snapshot: &'a contract::ImportWitnessSnapshot,
    bundle: &wire::ImportPublicProofBundleV1,
) -> Vec<&'a wire::ImportPublicProofBundleV1> {
    let terminal = bundle.terminal_manifest.as_ref();
    let spool = bundle
        .delegations
        .first()
        .and_then(|d| d.body.as_ref())
        .and_then(|d| d.identity.as_ref())
        .map(|id| &id.spool_uuid);
    snapshot
        .accepted_history
        .iter()
        .filter(|old| {
            old.terminal_manifest.as_ref().is_some_and(|m| {
                terminal.is_some_and(|terminal| m.logical_job_id == terminal.logical_job_id)
            }) && old
                .delegations
                .first()
                .and_then(|d| d.body.as_ref())
                .and_then(|d| d.identity.as_ref())
                .map(|id| &id.spool_uuid)
                == spool
        })
        .collect::<Vec<_>>()
}

/// Authenticate a witnessed bundle using receiver-selected root and owner facts.
/// Staging uses this without persistence; installation repeats it under the lock.
#[allow(clippy::too_many_arguments)]
pub fn verify_witnessed_import_bundle(
    bundle: &wire::ImportPublicProofBundleV1,
    authority: &impl AcceptedAuthority,
    pin: &contract::ImportWitnessRootPin,
    snapshot: Option<&contract::ImportWitnessSnapshot>,
    now_millis: i64,
    associations: &[(Vec<u8>, Vec<u8>)],
    forbidden: &[Vec<u8>],
    require_selection: impl Fn(&Selection<'_>) -> Result<()>,
) -> Result<contract::VerifiedImportBundleWitnesses> {
    let states = public_owners(bundle.into(), now_millis / 1000)?;
    let [signed] = bundle.delegations.as_slice() else {
        return Err(Reject::Canonical.into());
    };
    let id = signed
        .body
        .as_ref()
        .and_then(|b| b.identity.as_ref())
        .ok_or(Reject::Root)?;
    let selection = authority.for_witness(&host::HostedWitnessStatementV1 {
        spool_uuid: id.spool_uuid.clone(),
        spool_genesis_digest: id.spool_genesis_digest.clone(),
        owner_id: id.owner_id.clone(),
        owner_state_hash: id.owner_state_hash.clone(),
        ownership_transfer_sequence: id.ownership_transfer_sequence,
        ..Default::default()
    })?;
    require_selection(&selection)?;
    let facts = import_owner_facts(
        &states,
        selection,
        forbidden.to_vec(),
        &bundle.ownership_transfers,
    )?;
    let mut owner_at = |time: Option<i64>| {
        let fact = owner_fact_at(&facts, id, time)?;
        Ok(fact.expectation(associations))
    };
    let mut verify_policy =
        |bundle: &wire::ImportPublicProofBundleV1,
         statement: Option<&host::HostedWitnessStatementV1>| {
            let verify = || -> Result<()> {
                let statement = statement.ok_or(Reject::Scope)?;
                let selection = authority.for_witness(statement)?;
                require_selection(&selection)?;
                selection.keyring.verify_current_owner(
                    selection.owner,
                    statement.observed_at_unix_millis / 1000,
                    selection.limits,
                )?;
                if statement.policy_sequence == 0 {
                    return Ok(());
                }
                let mut chain = Vec::new();
                let mut digest = statement.policy_state_hash.as_slice();
                while digest != [0; 32] {
                    let record = bundle
                        .policies
                        .iter()
                        .find(|p| {
                            p.body
                                .as_ref()
                                .is_some_and(|b| b.policy_state_hash == digest)
                        })
                        .ok_or(Reject::Scope)?;
                    if chain.len() >= bundle.policies.len() {
                        return Err(Reject::Scope.into());
                    }
                    chain.push(record.clone());
                    digest = &record
                        .body
                        .as_ref()
                        .ok_or(Reject::Canonical)?
                        .expected_head
                        .as_ref()
                        .ok_or(Reject::Canonical)?
                        .state_hash;
                }
                chain.reverse();
                heddleco_capability_verifier::policy::verify_signed_policy_chain(
                    &chain, &selection.keyring.owner_genesis().spool_uuid(),
                    statement.ownership_transfer_sequence,
                    |owner_id, state, sequence| {
                        let selector = wire::SignedPolicyBody {
                            spool_uuid: statement.spool_uuid.clone(), owner_id: owner_id.to_vec(),
                            owner_state_hash: state.to_vec(), ownership_transfer_sequence: sequence,
                            ..Default::default()
                        };
                        let selection = authority.for_policy(&selector).map_err(|_| heddleco_capability_verifier::policy::OwnerGovernanceError::NotOwnerSigned)?;
                        Ok((selection.owner.authority_key().clone(), selection.owner.authority_public_keys()
                            .map(|key| hybrid_codec::key_id(&key).try_into().map_err(|_| heddleco_capability_verifier::policy::OwnerGovernanceError::NotOwnerSigned))
                            .collect::<std::result::Result<Vec<_>, _>>()?))
                    },
                ).map_err(|_| Reject::Scope)?;
                Ok(())
            };
            verify().map_err(|error| match error {
                Error::Hybrid(reason) => reason,
                _ => Reject::Scope,
            })
        };
    let verified = contract::verify_import_bundle_witnesses(
        bundle,
        pin,
        snapshot,
        now_millis,
        &mut owner_at,
        &mut verify_policy,
    )?;
    if verified.evidence != contract::ImportBundleEvidence::Witnessed {
        return Err(Reject::Scope.into());
    }
    if let Some(snapshot) = snapshot {
        let histories = import_job_history(snapshot, bundle);
        // A Thread projection can retain an older version of this same job.
        // The API checks its first matching history; check every other version
        // too, then advance all those projections together under this lock.
        for old in histories.iter().skip(1) {
            let mut previous = snapshot.clone();
            previous.accepted_history = vec![(*old).clone()];
            contract::verify_import_bundle_witnesses(
                bundle,
                pin,
                Some(&previous),
                now_millis,
                &mut owner_at,
                &mut verify_policy,
            )?;
        }
    }
    // Original bundles, job associations and fresh witness trust commit with
    // the installed records; rejection rolls every projection back together.
    Ok(verified)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_in(
    directory: &Path,
    bundle: &wire::ImportPublicProofBundleV1,
    native_records: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
    context: &TrustTransaction<'_>,
    recheck_path: Option<&super::foreign_dependencies::Recheck<'_>>,
) -> Result<Vec<ThreadReplica>> {
    if recheck_path.is_none() {
        authority.authorize_import(bundle, context.now_millis(), context)?;
    }
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
    let forbidden = context.forbidden_job_keys();
    for policy in &bundle.policies {
        let p = policy.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_policy(p)?;
        context.require_spool_selection(&selection)?;
        require_public_selection(bundle.into(), &owners, &selection)?;
        // A retained policy is historical evidence. The composition callback
        // checks its selected owner at the authenticated observation time.
        if p.spool_uuid != selection.keyring.owner_genesis().spool_uuid()
            || p.owner_id != selection.owner.owner_id()
            || p.ownership_transfer_sequence
                != selection.keyring.wire().ownership_transfers.len() as u64
        {
            return Err(Error::Hybrid(Reject::Root));
        }
        permission::verify_policy_record(policy, &[selection.owner])?;
    }
    verify_import_history(bundle, authority, context, recheck_path.is_some())?;
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
    let authors = crypto::writer_authority::WitnessedAuthors::from_import(
        bundle,
        context.set(),
        context.now_millis(),
    )?;
    let mut original_geneses = verification::OriginalGeneses::import(&bundle.genesis_witnesses)?;
    let verified = std::cell::RefCell::default();
    let fresh = super::foreign_dependencies::Recheck {
        path: &[],
        verified: &verified,
    };
    let foreign = super::foreign_dependencies::InstalledForeign::load(
        &originals,
        super::foreign_dependencies::Dependent {
            references: &bundle.foreign_dependencies,
            statements: &bundle.statements,
            authority: &bundle.authority_witnesses,
            landing: &bundle.landing_witnesses,
            spool: uuid::Uuid::from_bytes(initial_selection.keyring.owner_genesis().spool_uuid()),
            spool_genesis: initial_selection.spool_genesis_digest,
        },
        context,
        directory,
        store,
        authority,
        recheck_path.unwrap_or(&fresh),
    )?;
    foreign.add_geneses(&mut originals, &mut original_geneses)?;
    let scopes = delegations.values().map(|d| d.scope()).collect::<Vec<_>>();
    let mut closure = NativeClosure::verify_with_resolvers(
        &originals,
        &boundaries,
        |genesis, operation, parents| {
            verification::bind_import_original(bundle, &scopes, genesis, operation, parents)
        },
        |r| foreign.resolve(r),
    )?;
    let mut imports = BTreeMap::new();
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
        let import_result = bundle
            .operations
            .iter()
            .find(|o| {
                o.body
                    .as_ref()
                    .is_some_and(|o| o.genesis_digest == binding_body.genesis_digest)
            })
            .ok_or(Reject::Transition)?;
        let (publication_manifest, publication_statement) =
            find_publication(bundle, import_result)?;
        let publication_evidence =
            self::evidence(&bundle.history_proofs, context, publication_statement)?;
        let d = permission::verify_publication_admission(
            signed,
            member,
            &c,
            payload,
            statement,
            evidence.resolved(),
            Some(&permission::PublicationWitness {
                operation: import_result,
                manifest: publication_manifest,
                statement: publication_statement,
                proof: publication_evidence.proof(),
            }),
            context.set(),
            |observation, r| authority.import_revoked(observation, r),
        )?;
        let genesis = verification::verify_genesis_payload_at_boundary(
            payload,
            &evidence,
            &d,
            &closure,
            &verification::NativeAuthorityContext {
                author_authority: &authors,
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
                original_geneses: &original_geneses,
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
        if recheck_path.is_none() {
            context.retain_delegation(&d)?;
        }
        delegations
            .entry(contract::signed_delegation_digest(signed)?)
            .or_insert(d);
    }
    let mut statements = bundle.statements.iter().collect::<Vec<_>>();
    statements.sort_by_key(|s| s.body.as_ref().map(|s| (s.purpose == 4, s.admission_order)));
    for statement in statements {
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
            // Already admitted with its own atomic P3 in the genesis loop.
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
            author_authority: &authors,
            owner: selection.owner,
            spool_uuid: spool,
            spool_genesis: selection.spool_genesis_digest,
            transfer_sequence: selection.keyring.wire().ownership_transfers.len() as u64,
            spool_path: &path,
            witness_set: context.set(),
            original_geneses: &original_geneses,
            known_job_associations: &job_associations,
            forbidden_authority_keys: &forbidden,
        };
        if s.purpose == 2 {
            let p = bundle
                .authority_witnesses
                .iter()
                .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                .ok_or(Reject::Scope)?;
            verification::verify_authority_payload(p, &evidence, &mut closure, &native, |r| {
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
            verification::verify_landing_payload(p, &evidence, &mut closure, &native, |r| {
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
        if recheck_path.is_none() {
            context.retain_delegation(d)?;
            context.retain_slot(operation)?;
        }
        let Some(converted) = native_frontiers.get(&body.resulting_frontier_digest) else {
            continue;
        };
        let converted = *converted;
        let (_, native_operation) = verification::verify_native_operation(converted)?;
        let parents = native_operation
            .parents
            .iter()
            .map(|id| closure.operation(id).cloned())
            .collect::<verification::Result<Vec<ThreadOperation>>>()?;
        let content =
            verification::verify_delegated_import(operation, d, original, converted, &parents)?;
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
        imports.insert(native_operation.id()?, content);
        admissions.insert(native_operation.id()?, (converted.clone(), evidence));
    }
    let selected = selected_originals(native_records, &originals, &foreign.ids)?;
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
    if recheck_path.is_some() {
        recheck_selected_in(directory, &selected, &closure, store, context)?;
        return Ok(Vec::new());
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
        &imports,
        &foreign.ids,
        store,
        context,
        |id| retain_bundle(context, id, bundle),
    )
}

/// Authenticate enclosing import certificates before source staging. Original
/// content is still bound on arrival, and installation repeats durable checks.
#[allow(clippy::too_many_arguments)]
pub fn authenticate_import_carriers(
    bundle: &wire::ImportPublicProofBundleV1,
    authority: &impl AcceptedAuthority,
    pin: &contract::ImportWitnessRootPin,
    now_millis: i64,
    associations: &[(Vec<u8>, Vec<u8>)],
    forbidden: &[Vec<u8>],
    require_selection: impl Fn(&Selection<'_>) -> Result<()>,
) -> Result<verification::VerifiedImportCarriers> {
    verify_witnessed_import_bundle(
        bundle,
        authority,
        pin,
        None,
        now_millis,
        associations,
        forbidden,
        &require_selection,
    )?;
    let set = api::witness_trust::verify_set(
        bundle.witness_set.as_ref().ok_or(Reject::Canonical)?,
        &api::witness_trust::SetExpectation {
            authority: &pin.authority,
            root_id: &pin.root_id,
            root_public_key: &pin.public_key,
            root_epoch: pin.epoch,
            now_unix_millis: now_millis,
            clock_floor_unix_millis: 0,
            known_job_keys: &associations
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>(),
        },
        None,
    )?;
    let mut verified = BTreeMap::new();
    for operation in &bundle.operations {
        let digest = &operation
            .body
            .as_ref()
            .ok_or(Reject::Canonical)?
            .delegation_digest;
        let signed = bundle
            .delegations
            .iter()
            .find(|d| contract::signed_delegation_digest(d).is_ok_and(|d| &d == digest))
            .ok_or(Reject::Scope)?;
        let (_, statement) = find_publication(bundle, operation)?;
        let mut evidence = None;
        for proof in std::iter::once(None).chain(bundle.history_proofs.iter().map(Some)) {
            if let Ok(found) = WitnessEvidence::resolve(&set, statement, proof, false, now_millis) {
                evidence = Some(found);
                break;
            }
        }
        let evidence = evidence.ok_or(Reject::Proof)?;
        let observation = statement.body.as_ref().ok_or(Reject::Canonical)?;
        let selection = authority.for_witness(observation)?;
        require_selection(&selection)?;
        let member = contract::resolve_bundle_permission(
            bundle,
            &signed
                .body
                .as_ref()
                .ok_or(Reject::Canonical)?
                .parent_permission_digest,
        )?;
        let certificate = permission::verify_historical(
            signed,
            member,
            &CurrentContext {
                selection,
                now_millis,
                forbidden_job_keys: forbidden,
                known_job_associations: associations,
            },
            statement,
            evidence.resolved(),
            &set,
            |r| authority.import_revoked(observation, r),
        )?;
        verified.insert(digest.clone(), certificate.scope().clone());
    }
    Ok(verification::VerifiedImportCarriers::new(
        bundle.clone(),
        verified
            .into_values()
            .next()
            .ok_or(Reject::ImportPermission)?,
    )?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_selected_in(
    directory: &Path,
    selected: BTreeMap<ContentHash, wire::SignedRecord>,
    geneses: &BTreeMap<ContentHash, (crypto::thread_operation::SignedGenesis, Vec<u8>)>,
    admissions: &BTreeMap<ContentHash, (wire::SignedRecord, WitnessEvidence)>,
    closure: &NativeClosure,
    imports: &BTreeMap<
        ContentHash,
        objects::object::thread_replication::delegated_import::DelegatedImport,
    >,
    installed_foreign: &BTreeSet<ContentHash>,
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
            let op = closure.operation(id)?;
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
        let mut dependencies: BTreeSet<_> = dependencies
            .into_iter()
            .filter(|id| !installed_foreign.contains(id))
            .collect();
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
                // The closure already verified this exact original's bytes and
                // signature. Reuse its operation throughout installation.
                let operation = closure.operation(&id)?;
                let signed = crypto::thread_operation::SignedOperation {
                    canonical: record.canonical_record.clone(),
                    signature: record
                        .signatures
                        .first()
                        .ok_or(Reject::Signature)?
                        .signature
                        .clone(),
                };
                replica.validate_reference_capture_in(
                    context.sql(),
                    closure.genesis(&thread)?,
                    operation,
                    store,
                )?;
                if replica.receive_verified_content_in(
                    context.sql(),
                    &signed,
                    operation,
                    store,
                    false,
                    imports.get(&id),
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

// Reuse native closure checks on installed originals without admitting or
// publishing anything. Every selected row and signature must still be exact.
pub(super) fn recheck_selected_in(
    directory: &Path,
    selected: &BTreeMap<ContentHash, wire::SignedRecord>,
    closure: &NativeClosure,
    store: &impl ObjectStore,
    context: &TrustTransaction<'_>,
) -> Result<()> {
    for record in selected.values() {
        let (id, thread, _) = native_subject(record)?;
        super::foreign_dependencies::require_installed(context, record, id, thread)?;
        if record.format == objects::object::thread_replication::OPERATION_FORMAT {
            let operation = verification::verify_native_operation(record)?.1;
            let replica = ThreadReplica::open(directory, thread)?;
            replica.validate_reference_capture_in(
                context.sql(),
                closure.genesis(&thread)?,
                &operation,
                store,
            )?;
            replica.require_local_integration_source_in(context.sql(), &operation)?;
        }
    }
    Ok(())
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
    installed_foreign: &BTreeSet<ContentHash>,
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
        if installed_foreign.contains(&id) {
            continue;
        }
        if let Some(previous) = selected.insert(id, record.clone()) {
            if previous != record {
                return Err(Error::Hybrid(Reject::Canonical));
            }
            continue;
        }
        for dependency in dependencies {
            if installed_foreign.contains(&dependency) {
                continue;
            }
            pending.push((*available.get(&dependency).ok_or(Reject::Scope)?).clone());
        }
    }
    Ok(selected)
}

pub(super) fn retain_bundle(
    context: &TrustTransaction<'_>,
    thread: &ContentHash,
    bundle: &wire::ImportPublicProofBundleV1,
) -> Result<()> {
    let native: bool = context.sql().query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_native_proofs WHERE thread=?1)",
        [thread.as_bytes()],
        |r| r.get(0),
    )?;
    if native {
        return Err(Reject::Scope.into());
    }
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
                .foreign_dependencies
                .iter()
                .any(|r| !bundle.foreign_dependencies.contains(r))
            || old.member_permission != bundle.member_permission
            || old.manifests.iter().any(|m| !bundle.manifests.contains(m))
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
