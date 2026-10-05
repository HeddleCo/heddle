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
                install_in(directory, &bundle, records, authority, store, context, None)
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
    /// Pending bindings grant no hosted trust and are excluded from exports.
    /// Retry may replace one after independently selecting a new owner state.
    pub fn stage_native_genesis_binding(
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
        let final_binding: Option<Vec<u8>> = tx
            .query_row(
                "SELECT binding FROM hosted_native_genesis_bindings WHERE thread=?1",
                [self.thread.as_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        if final_binding.is_some_and(|b| b != binding.encode_to_vec()) {
            return Err(Reject::SlotConflict.into());
        }
        tx.execute(
            "INSERT INTO pending_native_genesis_bindings(thread,binding) VALUES(?1,?2) ON CONFLICT(thread) DO UPDATE SET binding=excluded.binding",
            params![self.thread.as_bytes(), binding.encode_to_vec()],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Producer retry state; never included in a public genesis record.
    pub fn pending_native_genesis_binding(
        &self,
    ) -> Result<Option<wire::SignedNativeGenesisAuthorityV1>> {
        let bytes: Option<Vec<u8>> = self
            .connect()?
            .query_row(
                "SELECT binding FROM pending_native_genesis_bindings WHERE thread=?1",
                [self.thread.as_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        bytes
            .map(|b| hybrid_codec::strict_decode(&b, 65536).map_err(Error::from))
            .transpose()
    }
    /// Finalize the producer binding after StartThread succeeds.
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

#[allow(clippy::too_many_arguments)]
pub(super) fn install_in(
    directory: &Path,
    bundle: &wire::NativePublicProofBundleV1,
    requested: &[wire::SignedRecord],
    authority: &impl AcceptedAuthority,
    store: &impl ObjectStore,
    context: &TrustTransaction<'_>,
    recheck_path: Option<&super::foreign_dependencies::Recheck<'_>>,
) -> Result<Vec<ThreadReplica>> {
    if recheck_path.is_none() {
        authority.authorize_native(bundle, context.now_millis(), context)?;
    }
    api::native_witness::verify_bundle_witnesses(
        bundle,
        context.set(),
        context.now_millis(),
        &context.forbidden_job_keys(),
    )?;
    let authors = crypto::writer_authority::WitnessedAuthors::from_native(
        bundle,
        context.set(),
        context.now_millis(),
    )?;
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
    let mut original_geneses = OriginalGeneses::native(&bundle.genesis_witnesses)?;
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
            spool: uuid::Uuid::from_bytes(initial.keyring.owner_genesis().spool_uuid()),
            spool_genesis: initial.spool_genesis_digest,
        },
        context,
        directory,
        store,
        authority,
        recheck_path.unwrap_or(&fresh),
    )?;
    foreign.add_geneses(&mut originals, &mut original_geneses)?;
    let mut closure = NativeClosure::verify_with_resolvers(
        &originals,
        &boundaries,
        |_, _, _| Ok(None),
        |r| foreign.resolve(r),
    )?;
    let mut geneses = BTreeMap::new();
    let mut admissions = BTreeMap::new();
    let mut first_admissions = BTreeMap::new();
    let forbidden = context.forbidden_job_keys();
    let mut statements = bundle.statements.iter().collect::<Vec<_>>();
    statements.sort_by_key(|s| s.body.as_ref().map(|s| (s.purpose == 4, s.admission_order)));
    for signed in statements {
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
            author_authority: &authors,
            owner: selected.owner,
            spool_uuid: uuid::Uuid::from_bytes(selected.keyring.owner_genesis().spool_uuid()),
            spool_genesis: selected.spool_genesis_digest,
            transfer_sequence: s.ownership_transfer_sequence,
            spool_path: &path,
            witness_set: context.set(),
            original_geneses: &original_geneses,
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
                admit_genesis(&mut geneses, id, signed, &p.creator_authority_envelope)?;
                if recheck_path.is_none() {
                    retain_binding(context.sql(), id, binding)?;
                }
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
                    &mut closure,
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
                    &mut closure,
                    &native_context,
                    |r| authority.native_revoked(s, r),
                )?;
                p.execution.as_ref().ok_or(Reject::Canonical)?
            }
            _ => return Err(Reject::Version.into()),
        };
        let id = delegated_import::native_subject(record)?.0;
        if first_admissions
            .insert((id, s.purpose), signed)
            .is_some_and(|previous| previous != signed)
        {
            return Err(Reject::SlotConflict.into());
        }
        if let Some((prior, _)) = admissions.insert(id, (record.clone(), evidence))
            && prior != *record
        {
            return Err(Reject::SlotConflict.into());
        }
    }
    let mut selected = delegated_import::selected_originals(requested, &originals, &foreign.ids)?;
    // Hosting a LocalKey requires the complete explicit ownership decision.
    for p in &bundle.authority_witnesses {
        if p.kind == 2 || p.kind == 3 {
            let record = p.original.as_ref().ok_or(Reject::Canonical)?;
            if selected.contains_key(&delegated_import::native_subject(record)?.1) {
                selected.extend(delegated_import::selected_originals(
                    std::slice::from_ref(record),
                    &originals,
                    &foreign.ids,
                )?);
            }
        }
    }
    let mut claims =
        BTreeMap::<_, BTreeMap<_, native::ownership_claim::ThreadOwnershipClaim>>::new();
    let mut resolutions =
        BTreeMap::<_, native::ownership_resolution::ThreadOwnershipResolution>::new();
    for payload in &bundle.authority_witnesses {
        let record = payload.original.as_ref().ok_or(Reject::Canonical)?;
        match payload.kind {
            2 => {
                let claim = native::ownership_claim::ThreadOwnershipClaim::decode(
                    &record.canonical_record,
                )?;
                claims
                    .entry(claim.thread)
                    .or_default()
                    .insert(claim.id()?, claim);
            }
            3 => {
                let resolution = native::ownership_resolution::ThreadOwnershipResolution::decode(
                    &record.canonical_record,
                )?;
                if let Some(previous) = resolutions.insert(resolution.thread, resolution.clone())
                    && previous != resolution
                {
                    return Err(Reject::SlotConflict.into());
                }
            }
            _ => {}
        }
    }
    let mut claimed_local_work = BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for (thread, mut pending) in ownership_cutoffs(&claims, &resolutions)? {
        let covered = claimed_local_work.entry(thread).or_default();
        while let Some(id) = pending.pop() {
            let operation = closure.operation(&id)?;
            // Cross-Thread source work is authorized by its own claim and
            // cutoff, never by walking another Thread's accepted frontier.
            if operation.thread == thread && covered.insert(id) {
                pending.extend(operation.parents.iter().copied());
            }
        }
    }
    for (id, record) in &selected {
        if record.format == native::OPERATION_FORMAT {
            let op = closure.operation(id)?;
            if dependency_role(op)? == NativeRole::LocalWork {
                let genesis = closure.genesis(&op.thread)?;
                if genesis.owner != GenesisOwner::LocalKey(op.publisher) {
                    return Err(Reject::Scope.into());
                }
                if !claimed_local_work
                    .get(&op.thread)
                    .is_some_and(|covered| covered.contains(id))
                {
                    return Err(Reject::Scope.into());
                }
                continue;
            }
        }
        if !admissions
            .get(id)
            .is_some_and(|(original, _)| original == record)
        {
            return Err(Reject::Scope.into());
        }
    }
    if recheck_path.is_some() {
        delegated_import::recheck_selected_in(directory, &selected, &closure, store, context)?;
        return Ok(Vec::new());
    }
    delegated_import::install_selected_in(
        directory,
        selected,
        &geneses,
        &admissions,
        &closure,
        &BTreeMap::new(),
        &foreign.ids,
        store,
        context,
        |thread| retain_bundle(context, *thread, bundle),
    )
}

fn admit_genesis(
    geneses: &mut BTreeMap<ContentHash, (crypto::thread_operation::SignedGenesis, Vec<u8>)>,
    id: ContentHash,
    signed: crypto::thread_operation::SignedGenesis,
    envelope: &[u8],
) -> Result<()> {
    if geneses.insert(id, (signed, envelope.to_vec())).is_some() {
        return Err(Reject::SlotConflict.into());
    }
    Ok(())
}

pub(super) fn ownership_cutoffs(
    claims: &BTreeMap<
        ContentHash,
        BTreeMap<ContentHash, native::ownership_claim::ThreadOwnershipClaim>,
    >,
    resolutions: &BTreeMap<ContentHash, native::ownership_resolution::ThreadOwnershipResolution>,
) -> Result<BTreeMap<ContentHash, Vec<ContentHash>>> {
    if resolutions
        .keys()
        .any(|thread| !claims.contains_key(thread))
    {
        return Err(Reject::Scope.into());
    }
    let mut cutoffs = BTreeMap::new();
    for (thread, claims) in claims {
        let pending = match resolutions.get(thread) {
            Some(resolution) => {
                if resolution.conflicting_claims != claims.keys().copied().collect()
                    || !claims.contains_key(&resolution.winning_claim)
                {
                    return Err(Reject::Scope.into());
                }
                resolution.frontier.iter().copied().collect::<Vec<_>>()
            }
            None if claims.len() == 1 => claims
                .values()
                .next()
                .ok_or(Reject::Scope)?
                .source_frontier
                .iter()
                .copied()
                .collect(),
            None => return Err(Reject::Scope.into()),
        };
        cutoffs.insert(*thread, pending);
    }
    Ok(cutoffs)
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
    sql.execute(
        "DELETE FROM pending_native_genesis_bindings WHERE thread=?1",
        [thread.as_bytes()],
    )?;
    Ok(())
}
pub(super) fn retain_bundle(
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
                .foreign_dependencies
                .iter()
                .any(|r| !bundle.foreign_dependencies.contains(r))
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

#[cfg(test)]
#[path = "native_witness_tests.rs"]
mod tests;
