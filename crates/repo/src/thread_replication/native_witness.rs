//! Native hosted installation through the same atomic facade as imports.
use std::{collections::BTreeMap, path::Path};

use api::{
    heddle::api::v1alpha2 as wire,
    hybrid_codec::{self, Reject},
};
use crypto::import_authority::{self as verification, NativeClosure, OriginalGeneses};
use objects::{
    object::{
        ContentHash,
        thread_replication::{self as native, GenesisOwner, SourceAuthor},
    },
    store::ObjectStore,
};
use prost::Message;
use rusqlite::{OptionalExtension, params};

use super::{
    Error, Result, ThreadReplica,
    delegated_import::{self, AcceptedAuthority},
    hosted_trust::{Clock, HostedTrust, TrustTransaction},
    install_artifacts::InstallArtifacts,
};

impl ThreadReplica {
    /// Fresh independently selected roots, disclosure, and exact native role
    /// evidence are rechecked within the serialized artifact transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn install_hybrid_native(
        directory: &Path,
        trust: &HostedTrust<impl Clock>,
        bytes: &[u8],
        records: &[wire::SignedRecord],
        authority: &impl AcceptedAuthority,
        store: &impl ObjectStore,
        publish: impl FnOnce(&mut InstallArtifacts<'_>) -> Result<()>,
    ) -> Result<Vec<Self>> {
        Self::install_hybrid_native_with(
            directory,
            trust,
            bytes,
            records,
            authority,
            store,
            |_| Ok(()),
            |_, a| publish(a),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn install_hybrid_native_with(
        directory: &Path,
        trust: &HostedTrust<impl Clock>,
        bytes: &[u8],
        records: &[wire::SignedRecord],
        authority: &impl AcceptedAuthority,
        store: &impl ObjectStore,
        before: impl FnOnce(&TrustTransaction<'_>) -> Result<()>,
        publish: impl FnOnce(&TrustTransaction<'_>, &mut InstallArtifacts<'_>) -> Result<()>,
    ) -> Result<Vec<Self>> {
        if directory.canonicalize()? != trust.directory().canonicalize()? {
            return Err(Reject::Root.into());
        }
        let bundle: wire::NativePublicProofBundleV1 =
            hybrid_codec::strict_decode(bytes, api::import_authority::MAX_BUNDLE_BYTES)?;
        api::native_witness::validate_public_bundle(&bundle)?;
        let replicas = trust.mutate_with_artifacts(
            bundle.witness_set.as_ref().ok_or(Reject::Root)?,
            |context| {
                before(context)?;
                install_in(directory, &bundle, records, authority, store, context)
            },
            |context, now| authority.authorize_native(&bundle, now, context),
            publish,
            |context, now| authority.authorize_native(&bundle, now, context),
        )?;
        if let Some(replica) = replicas.first() {
            replica.notify_committed()?;
        }
        Ok(replicas)
    }
    /// Complete retained native history, including originals and creator bindings.
    pub fn hybrid_native_bundle(&self) -> Result<Option<wire::NativePublicProofBundleV1>> {
        let bytes: Option<Vec<u8>> = self
            .connect()?
            .query_row(
                "SELECT bundle FROM hosted_native_proofs WHERE thread=?1",
                [self.thread.as_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        bytes
            .map(|b| {
                hybrid_codec::strict_decode(&b, api::import_authority::MAX_BUNDLE_BYTES)
                    .map_err(Error::from)
            })
            .transpose()
    }
    /// Retain the producer's frozen creator binding, never rewrite first admission.
    pub fn retain_native_genesis_binding(
        &self,
        binding: &wire::SignedNativeGenesisAuthorityV1,
    ) -> Result<()> {
        let record = self.genesis_record()?;
        api::native_witness::verify_genesis_authority(
            binding,
            record.genesis.as_ref().ok_or(Reject::Canonical)?,
            &record.creator_authority,
        )?;
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        retain_binding(&tx, self.thread, binding)?;
        tx.commit()?;
        Ok(())
    }
}

fn install_in(
    directory: &Path,
    bundle: &wire::NativePublicProofBundleV1,
    requested: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
    context: &TrustTransaction<'_>,
) -> Result<Vec<ThreadReplica>> {
    authority.authorize_native(bundle, context.now_millis(), context)?;
    api::native_witness::verify_bundle_witnesses(bundle, context.set(), context.now_millis())?;
    let owners = delegated_import::public_owners(bundle.into(), context.now_millis() / 1000)?;
    let first = bundle
        .statements
        .first()
        .and_then(|s| s.body.as_ref())
        .ok_or(Reject::Canonical)?;
    let initial = authority.for_witness(first)?;
    context.require_spool_selection(&initial)?;
    delegated_import::require_public_selection(bundle.into(), &owners, &initial)?;
    delegated_import::verify_complete_transfer_history(
        bundle.into(),
        &initial,
        context.now_millis() / 1000,
    )?;
    for policy in &bundle.policies {
        let p = policy.body.as_ref().ok_or(Reject::Canonical)?;
        let selected = authority.for_policy(p)?;
        context.require_spool_selection(&selected)?;
        delegated_import::require_public_selection(bundle.into(), &owners, &selected)?;
        if p.spool_uuid != selected.keyring.owner_genesis().spool_uuid()
            || p.owner_id != selected.owner.owner_id()
            || p.ownership_transfer_sequence
                != selected.keyring.wire().ownership_transfers.len() as u64
        {
            return Err(Reject::Root.into());
        }
        heddleco_capability_verifier::import_delegation::verify_policy_record(
            policy,
            &[selected.owner],
        )?;
    }
    let mut originals = requested.to_vec();
    for p in &bundle.genesis_witnesses {
        originals.extend(p.original_genesis.iter().cloned());
    }
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
    let mut geneses = BTreeMap::new();
    let mut admissions = BTreeMap::new();
    let forbidden = context.forbidden_job_keys();
    for signed in &bundle.statements {
        let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
        let evidence = delegated_import::evidence(&bundle.history_proofs, context, signed)?;
        let selected = authority.for_witness(s)?;
        context.require_spool_selection(&selected)?;
        delegated_import::require_public_selection(bundle.into(), &owners, &selected)?;
        selected.keyring.verify_current_owner(
            selected.owner,
            s.observed_at_unix_millis / 1000,
            selected.limits,
        )?;
        if s.spool_uuid != selected.keyring.owner_genesis().spool_uuid()
            || s.spool_genesis_digest != selected.spool_genesis_digest
            || s.owner_id != selected.owner.owner_id()
            || s.owner_state_hash != selected.owner.state_hash()
            || s.ownership_transfer_sequence
                != selected.keyring.wire().ownership_transfers.len() as u64
        {
            return Err(Reject::Root.into());
        }
        let path = selected
            .keyring
            .wire()
            .canonical_spool_path_segments
            .join("/");
        let native_context = verification::NativeAuthorityContext {
            owner: selected.owner,
            spool_uuid: uuid::Uuid::from_bytes(selected.keyring.owner_genesis().spool_uuid()),
            spool_genesis: selected.spool_genesis_digest,
            transfer_sequence: s.ownership_transfer_sequence,
            spool_path: &path,
            witness_set: context.set(),
            original_geneses: OriginalGeneses::Native(&bundle.genesis_witnesses),
            known_job_associations: context.job_associations(),
            forbidden_authority_keys: &forbidden,
        };
        let record = match s.purpose {
            1 => {
                let p = bundle
                    .genesis_witnesses
                    .iter()
                    .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                    .ok_or(Reject::Scope)?;
                let binding = p.binding.as_ref().ok_or(Reject::GenesisBinding)?;
                let binding_selection = authority
                    .for_native_binding(binding.body.as_ref().ok_or(Reject::GenesisBinding)?)?;
                context.require_spool_selection(&binding_selection)?;
                delegated_import::require_public_selection(
                    bundle.into(),
                    &owners,
                    &binding_selection,
                )?;
                let original = p.original_genesis.as_ref().ok_or(Reject::Canonical)?;
                heddleco_capability_verifier::native_genesis::verify_binding(
                    binding,
                    original,
                    &p.creator_authority_envelope,
                    &binding_selection,
                )?;
                let signed = crypto::native_witness::verify_genesis_payload(
                    p,
                    &evidence,
                    &binding_selection,
                    &closure,
                    &native_context,
                    |r| authority.native_revoked(s, r),
                )?;
                let id = signed.verify()?.id()?;
                if geneses
                    .insert(id, (signed, p.creator_authority_envelope.clone()))
                    .is_some()
                {
                    return Err(Reject::SlotConflict.into());
                }
                retain_binding(context.sql(), id, binding)?;
                delegated_import::admit_boundary_originals(
                    &mut admissions,
                    p.boundary_acceptance.as_slice(),
                    &originals,
                    &evidence,
                )?;
                original
            }
            2 => {
                let p = bundle
                    .authority_witnesses
                    .iter()
                    .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                    .ok_or(Reject::Scope)?;
                verification::verify_authority_payload(
                    p,
                    &evidence,
                    &closure,
                    &native_context,
                    |r| authority.native_revoked(s, r),
                )?;
                delegated_import::admit_boundary_originals(
                    &mut admissions,
                    &p.boundary_acceptances,
                    &originals,
                    &evidence,
                )?;
                p.original.as_ref().ok_or(Reject::Canonical)?
            }
            4 => {
                let p = bundle
                    .landing_witnesses
                    .iter()
                    .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                    .ok_or(Reject::Scope)?;
                verification::verify_landing_payload(
                    p,
                    &evidence,
                    &closure,
                    &native_context,
                    |r| authority.native_revoked(s, r),
                )?;
                p.execution.as_ref().ok_or(Reject::Canonical)?
            }
            _ => return Err(Reject::Version.into()),
        };
        let id = delegated_import::native_subject(record)?.0;
        if let Some((prior, _)) = admissions.insert(id, (record.clone(), evidence))
            && prior != *record
        {
            return Err(Reject::SlotConflict.into());
        }
    }
    let mut selected = delegated_import::selected_originals(requested, &originals)?;
    // Hosting a LocalKey requires the complete explicit ownership decision.
    for p in &bundle.authority_witnesses {
        if p.kind == 2 || p.kind == 3 {
            let record = p.original.as_ref().ok_or(Reject::Canonical)?;
            if selected.contains_key(&delegated_import::native_subject(record)?.1) {
                selected.extend(delegated_import::selected_originals(
                    std::slice::from_ref(record),
                    &originals,
                )?);
            }
        }
    }
    let mut claimed_local_work = std::collections::BTreeSet::new();
    let mut pending = Vec::new();
    for payload in &bundle.authority_witnesses {
        if payload.kind == 2 {
            let claim = native::ownership_claim::ThreadOwnershipClaim::decode(
                &payload
                    .original
                    .as_ref()
                    .ok_or(Reject::Canonical)?
                    .canonical_record,
            )?;
            pending.extend(claim.source_frontier);
        } else if payload.kind == 3 {
            let resolution = native::ownership_resolution::ThreadOwnershipResolution::decode(
                &payload
                    .original
                    .as_ref()
                    .ok_or(Reject::Canonical)?
                    .canonical_record,
            )?;
            pending.extend(resolution.frontier);
        }
    }
    while let Some(id) = pending.pop() {
        if claimed_local_work.insert(id) {
            pending.extend(closure.operation(&id)?.parents.iter().copied());
        }
    }
    for (id, record) in &selected {
        if admissions
            .get(id)
            .is_some_and(|(original, _)| original == record)
        {
            continue;
        }
        let (_, op) = verification::verify_native_operation(record)?;
        if dependency_role(&op)? != NativeRole::LocalWork || !claimed_local_work.contains(id) {
            return Err(Reject::Scope.into());
        }
        let genesis = closure.genesis(&op.thread)?;
        if !matches!(&genesis.owner, GenesisOwner::LocalKey(key) if key == &genesis.creator && key == &op.publisher)
            || !bundle.authority_witnesses.iter().any(|p| {
                p.kind == 2
                    && p.original.as_ref().is_some_and(|c| {
                        delegated_import::native_subject(c)
                            .is_ok_and(|(_, thread, _)| thread == op.thread)
                    })
            })
        {
            return Err(Reject::Scope.into());
        }
    }
    delegated_import::install_selected_in(
        directory,
        selected,
        &geneses,
        &admissions,
        &closure,
        store,
        context,
        |thread| retain_bundle(context, *thread, bundle),
    )
}

#[derive(PartialEq)]
enum NativeRole {
    Authority,
    Landing,
    LocalWork,
}
fn dependency_role(op: &native::ThreadOperation) -> Result<NativeRole> {
    if op.integration()?.is_some() {
        return Ok(NativeRole::Landing);
    }
    if matches!(op.source_author()?, Some(SourceAuthor::LocalKey)) {
        return Ok(NativeRole::LocalWork);
    }
    Ok(NativeRole::Authority)
}

fn retain_binding(
    sql: &rusqlite::Transaction<'_>,
    thread: ContentHash,
    binding: &wire::SignedNativeGenesisAuthorityV1,
) -> Result<()> {
    let bytes = binding.encode_to_vec();
    let old: Option<Vec<u8>> = sql
        .query_row(
            "SELECT binding FROM hosted_native_genesis_bindings WHERE thread=?1",
            [thread.as_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    if old.is_some_and(|old| old != bytes) {
        return Err(Reject::SlotConflict.into());
    }
    sql.execute(
        "INSERT OR IGNORE INTO hosted_native_genesis_bindings(thread,binding) VALUES(?1,?2)",
        params![thread.as_bytes(), bytes],
    )?;
    Ok(())
}
fn retain_bundle(
    context: &TrustTransaction<'_>,
    thread: ContentHash,
    bundle: &wire::NativePublicProofBundleV1,
) -> Result<()> {
    let imported: bool = context.sql().query_row(
        "SELECT EXISTS(SELECT 1 FROM hosted_import_proofs WHERE thread=?1)",
        [thread.as_bytes()],
        |r| r.get(0),
    )?;
    if imported {
        return Err(Reject::Scope.into());
    }
    let old: Option<Vec<u8>> = context
        .sql()
        .query_row(
            "SELECT bundle FROM hosted_native_proofs WHERE thread=?1",
            [thread.as_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(bytes) = old {
        let old: wire::NativePublicProofBundleV1 =
            hybrid_codec::strict_decode(&bytes, api::import_authority::MAX_BUNDLE_BYTES)?;
        if old.owner_genesis != bundle.owner_genesis
            || !bundle
                .ownership_transfers
                .starts_with(&old.ownership_transfers)
            || old
                .owner_histories
                .iter()
                .any(|p| !bundle.owner_histories.contains(p))
            || old
                .owner_chains
                .iter()
                .any(|p| !bundle.owner_chains.contains(p))
            || old.policies.iter().any(|p| !bundle.policies.contains(p))
            || old
                .genesis_witnesses
                .iter()
                .any(|p| !bundle.genesis_witnesses.contains(p))
            || old
                .authority_witnesses
                .iter()
                .any(|p| !bundle.authority_witnesses.contains(p))
            || old
                .landing_witnesses
                .iter()
                .any(|p| !bundle.landing_witnesses.contains(p))
            || old
                .statements
                .iter()
                .any(|p| !bundle.statements.contains(p))
        {
            return Err(Reject::SlotConflict.into());
        }
    }
    context.sql().execute("INSERT INTO hosted_native_proofs(thread,authority,bundle) VALUES(?1,?2,?3) ON CONFLICT(thread) DO UPDATE SET bundle=excluded.bundle", params![thread.as_bytes(), context.set().body().deployment_authority, bundle.encode_to_vec()])?;
    Ok(())
}
