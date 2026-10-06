//! Foreign references resolve only through this receiver's installed journal.
//! The resolver is constructed inside installation's trust transaction; callers
//! cannot supply an origin, an admitted original, or an import exception.
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec::{self, Reject},
    import_authority::{self as contract, WitnessPayload},
};
use crypto::import_authority::{self as verify, ForeignOriginal, OriginalGeneses};
use objects::object::{
    ContentHash,
    thread_replication::{self as native, ThreadOperation},
};
use prost::Message;
use rusqlite::{OptionalExtension, params};

use super::{Result, delegated_import, hosted_trust::TrustTransaction};

pub(super) struct Dependent<'a> {
    pub references: &'a [wire::ForeignDependencyV1],
    pub statements: &'a [host::SignedHostedWitnessStatementV1],
    pub authority: &'a [wire::ImportAuthorityWitnessV1],
    pub landing: &'a [wire::HostedLandingWitnessV1],
    pub spool: uuid::Uuid,
    pub spool_genesis: &'a [u8; 32],
}
pub(super) struct Recheck<'a> {
    pub path: &'a [wire::ForeignDependencyV1],
    pub verified: &'a RefCell<BTreeSet<Vec<u8>>>,
    pub budget: &'a RefCell<ForeignPrefixBudget>,
}

/// Bound uncached Fetch work across siblings and depth for Fetch and retained replay.
#[derive(Default)]
pub struct ForeignPrefixBudget {
    visited: usize,
}
impl ForeignPrefixBudget {
    pub const MAX_DEPTH: usize = 32;
    pub const MAX_PREFIXES: usize = 256;

    /// Installed endpoints spend no Fetch count, but retained replay still obeys
    /// the depth bound and revalidates authority under the current trust lock.
    pub fn visit(&mut self, depth: usize, installed: bool) -> Result<()> {
        let (limit_name, limit) = if depth > Self::MAX_DEPTH {
            ("depth", Self::MAX_DEPTH)
        } else if !installed && self.visited >= Self::MAX_PREFIXES {
            ("count", Self::MAX_PREFIXES)
        } else {
            self.visited += usize::from(!installed);
            return Ok(());
        };
        Err(super::Error::ForeignPrefixLimitExceeded { limit_name, limit })
    }
}
type RetainedOrigins = (Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Default)]
pub(super) struct InstalledForeign {
    originals: BTreeMap<Vec<u8>, ForeignOriginal>,
    pub ids: BTreeSet<ContentHash>,
    geneses: Vec<(wire::SignedRecord, Vec<u8>, wire::ForeignDependencyOrigin)>,
}
impl InstalledForeign {
    pub fn resolve(&self, record: &wire::SignedRecord) -> verify::Result<Option<ForeignOriginal>> {
        let digest = contract::signed_native_digest(record)?;
        match self.originals.get(&digest) {
            Some(original) if original.original != *record => Err(Reject::Scope.into()),
            value => Ok(value.cloned()),
        }
    }
    pub fn add_geneses(
        &self,
        originals: &mut Vec<wire::SignedRecord>,
        envelopes: &mut OriginalGeneses,
    ) -> Result<()> {
        for (record, envelope, origin) in &self.geneses {
            envelopes.insert(record, envelope, *origin)?;
            if !originals.contains(record) {
                originals.push(record.clone());
            }
        }
        Ok(())
    }
    /// Recheck every referenced endpoint before the dependent installer writes
    /// genesis, admissions, operations, proofs or filesystem artifacts.
    pub fn load(
        originals: &[wire::SignedRecord],
        dependent: Dependent<'_>,
        context: &TrustTransaction<'_>,
        directory: &Path,
        store: &impl objects::store::ObjectStore,
        authority: &impl delegated_import::AcceptedAuthority,
        check: &Recheck<'_>,
    ) -> Result<Self> {
        let Dependent {
            references,
            statements: dependent_statements,
            authority: dependent_authority,
            landing: dependent_landing,
            spool,
            spool_genesis,
        } = dependent;
        let mut result = Self::default();
        for reference in references {
            if check.path.contains(reference) {
                return Err(Reject::Scope.into());
            }
            let record = originals
                .iter()
                .find(|r| {
                    contract::signed_native_digest(r)
                        .is_ok_and(|d| d == reference.signed_native_digest)
                })
                .ok_or(Reject::Scope)?;
            let (id, thread, _) = delegated_import::native_subject(record)?;
            if thread.as_bytes().as_slice() != reference.thread_genesis_digest {
                return Err(Reject::Scope.into());
            }
            let (genesis_record, envelope, genesis) = installed_genesis(context, thread)?;
            if genesis.spool != spool.to_string() {
                return Err(Reject::Scope.into());
            }
            if id == thread {
                if *record != genesis_record {
                    return Err(Reject::Scope.into());
                }
            } else if installed_subject(context, id, thread, &record.format)? != *record {
                return Err(Reject::Scope.into());
            }
            let (imported, native) = retained_origin(context, thread)?;
            let (bound, cutoff) = match (reference.origin, imported, native) {
                (1, Some(bytes), None) => {
                    let bundle: wire::ImportPublicProofBundleV1 =
                        hybrid_codec::strict_decode(&bytes, contract::MAX_BUNDLE_BYTES)?;
                    contract::validate_public_bundle(&bundle)?;
                    let (bound, statement) =
                        imported_admission(&bundle, record, &genesis, context)?;
                    let cutoff = statement.admission_order;
                    recheck_prefix(
                        bundle.into(),
                        record,
                        reference,
                        dependent_statements,
                        dependent_authority,
                        dependent_landing,
                        spool_genesis,
                        directory,
                        store,
                        authority,
                        context,
                        check,
                    )?;
                    (bound, cutoff)
                }
                (2, None, Some(bytes)) => {
                    let bundle: wire::NativePublicProofBundleV1 =
                        hybrid_codec::strict_decode(&bytes, contract::MAX_BUNDLE_BYTES)?;
                    api::native_witness::validate_public_bundle(&bundle)?;
                    let cutoff = if record.format == native::OPERATION_FORMAT
                        && matches!(
                            verify::verify_native_operation(record)?.1.source_author()?,
                            Some(native::SourceAuthor::LocalKey)
                        ) {
                        let mut cutoffs = BTreeSet::new();
                        for signed in dependent_statements {
                            let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
                            if refers_to(s, record, dependent_authority, dependent_landing)? {
                                cutoffs.insert(api::native_witness::local_work_cutoff(
                                    &bundle,
                                    record,
                                    s.admission_order,
                                )?);
                            }
                        }
                        if cutoffs.len() != 1 {
                            return Err(Reject::Scope.into());
                        }
                        let cutoff = *cutoffs.first().ok_or(Reject::Scope)?;
                        // Local work has no P2. Its exact P1 and as-of ownership
                        // evidence survive only while their witnesses still do.
                        for signed in &bundle.statements {
                            let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
                            if s.admission_order <= cutoff {
                                delegated_import::evidence(
                                    &bundle.history_proofs,
                                    context,
                                    signed,
                                )?
                                .recheck(context.set(), context.now_millis())?;
                            }
                        }
                        cutoff
                    } else {
                        let signed = native_admission(&bundle, record)?;
                        recheck_admission(context, id, signed, &bundle.history_proofs)?;
                        signed
                            .body
                            .as_ref()
                            .ok_or(Reject::Canonical)?
                            .admission_order
                    };
                    recheck_prefix(
                        bundle.into(),
                        record,
                        reference,
                        dependent_statements,
                        dependent_authority,
                        dependent_landing,
                        spool_genesis,
                        directory,
                        store,
                        authority,
                        context,
                        check,
                    )?;
                    (None, cutoff)
                }
                _ => return Err(Reject::Scope.into()),
            };
            if cutoff != reference.prefix_admission_order {
                return Err(Reject::Scope.into());
            }
            let origin = wire::ForeignDependencyOrigin::try_from(reference.origin)
                .map_err(|_| Reject::Scope)?;
            result.ids.extend([id, thread]);
            result.originals.insert(
                reference.signed_native_digest.clone(),
                ForeignOriginal {
                    original: record.clone(),
                    genesis: genesis.clone(),
                    import: bound,
                },
            );
            result.originals.insert(
                contract::signed_native_digest(&genesis_record)?,
                ForeignOriginal {
                    original: genesis_record.clone(),
                    genesis,
                    import: None,
                },
            );
            result.geneses.push((genesis_record, envelope, origin));
        }
        Ok(result)
    }
}

fn refers_to(
    statement: &host::HostedWitnessStatementV1,
    record: &wire::SignedRecord,
    authority: &[wire::ImportAuthorityWitnessV1],
    landing: &[wire::HostedLandingWitnessV1],
) -> Result<bool> {
    match statement.purpose {
        2 => Ok(authority.iter().any(|p| {
            p.dependencies.contains(record)
                && hybrid_codec::canonical(p).is_ok_and(|b| b == statement.canonical_payload)
        })),
        4 => Ok(landing.iter().any(|p| {
            (p.source_operation.as_ref() == Some(record) || p.review_evidence.contains(record))
                && hybrid_codec::canonical(p).is_ok_and(|b| b == statement.canonical_payload)
        })),
        _ => Ok(false),
    }
}

fn installed_genesis(
    context: &TrustTransaction<'_>,
    thread: ContentHash,
) -> Result<(wire::SignedRecord, Vec<u8>, native::ThreadGenesis)> {
    let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = context
        .sql()
        .query_row(
            "SELECT genesis,genesis_signature,creator_authority FROM threads WHERE id=?1",
            [thread.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (canonical, signature, envelope) = row.ok_or(Reject::Scope)?;
    let genesis = native::ThreadGenesis::decode(&canonical)?;
    let record = wire::SignedRecord {
        format: native::GENESIS_FORMAT.into(),
        canonical_record: canonical,
        signatures: vec![wire::RecordSignature {
            public_key: genesis.creator.to_vec(),
            signature,
        }],
    };
    let (_, verified) = verify::verify_native_genesis(&record)?;
    if verified.id()? != thread {
        return Err(Reject::Scope.into());
    }
    Ok((record, envelope, verified))
}
fn installed_operation(
    context: &TrustTransaction<'_>,
    id: ContentHash,
    thread: ContentHash,
) -> Result<wire::SignedRecord> {
    let row: Option<(Vec<u8>, Vec<u8>)> = context
        .sql()
        .query_row(
            "SELECT canonical,signature FROM operations WHERE id=?1 AND thread=?2 AND status=1",
            params![id.as_bytes(), thread.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (canonical, signature) = row.ok_or(Reject::Scope)?;
    let op = ThreadOperation::decode(&canonical)?;
    let record = wire::SignedRecord {
        format: native::OPERATION_FORMAT.into(),
        canonical_record: canonical,
        signatures: vec![wire::RecordSignature {
            public_key: op.publisher.to_vec(),
            signature,
        }],
    };
    let (_, verified) = verify::verify_native_operation(&record)?;
    if verified.id()? != id || verified.thread != thread {
        return Err(Reject::Scope.into());
    }
    Ok(record)
}
fn installed_subject(
    context: &TrustTransaction<'_>,
    id: ContentHash,
    thread: ContentHash,
    format: &str,
) -> Result<wire::SignedRecord> {
    if format == native::OPERATION_FORMAT {
        return installed_operation(context, id, thread);
    }
    let (table, key) = match format {
        native::ownership_claim::FORMAT => ("thread_owner_claims", "id"),
        native::ownership_resolution::FORMAT => ("thread_owner_resolutions", "id"),
        _ => return Err(Reject::Scope.into()),
    };
    let sql = format!(
        "SELECT canonical,local_signature,acceptance_signature FROM {table} WHERE thread=?1 AND {key}=?2"
    );
    let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = context
        .sql()
        .query_row(&sql, params![thread.as_bytes(), id.as_bytes()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let (canonical, local_signature, acceptance_signature) = row.ok_or(Reject::Scope)?;
    let (local, acceptor) = if format == native::ownership_claim::FORMAT {
        let claim = native::ownership_claim::ThreadOwnershipClaim::decode(&canonical)?;
        (claim.prior_local_key, claim.accepting_publisher)
    } else {
        let resolution =
            native::ownership_resolution::ThreadOwnershipResolution::decode(&canonical)?;
        (resolution.local_owner, resolution.accepting_publisher)
    };
    let mut signatures = vec![
        wire::RecordSignature {
            public_key: local.to_vec(),
            signature: local_signature,
        },
        wire::RecordSignature {
            public_key: acceptor.to_vec(),
            signature: acceptance_signature,
        },
    ];
    signatures.sort_by(|a, b| a.public_key.cmp(&b.public_key));
    let record = wire::SignedRecord {
        format: format.into(),
        canonical_record: canonical,
        signatures,
    };
    if delegated_import::native_subject(&record)?.0 != id {
        return Err(Reject::Scope.into());
    }
    Ok(record)
}

fn retained_origin(context: &TrustTransaction<'_>, thread: ContentHash) -> Result<RetainedOrigins> {
    let load = |table: &str| -> Result<Option<Vec<u8>>> {
        let sql = format!("SELECT authority,bundle FROM {table} WHERE thread=?1");
        let row: Option<(String, Vec<u8>)> = context
            .sql()
            .query_row(&sql, [thread.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            Some((authority, _)) if authority != context.set().body().deployment_authority => {
                Err(Reject::Scope.into())
            }
            Some((_, bytes)) => Ok(Some(bytes)),
            None => Ok(None),
        }
    };
    Ok((load("hosted_import_proofs")?, load("hosted_native_proofs")?))
}
fn recheck_admission(
    context: &TrustTransaction<'_>,
    id: ContentHash,
    signed: &host::SignedHostedWitnessStatementV1,
    proofs: &[host::HostedWitnessHistoryProofV1],
) -> Result<()> {
    let row: Option<(String, Vec<u8>, Option<Vec<u8>>)> = context
        .sql()
        .query_row(
            "SELECT authority,statement,proof FROM hosted_import_admissions WHERE operation=?1",
            [id.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (authority, bytes, proof) = row.ok_or(Reject::Scope)?;
    if authority != context.set().body().deployment_authority || bytes != signed.encode_to_vec() {
        return Err(Reject::Scope.into());
    }
    let mut proofs = proofs.to_vec();
    if let Some(bytes) = proof {
        proofs.push(hybrid_codec::strict_decode(&bytes, 4096)?);
    }
    delegated_import::evidence(&proofs, context, signed)?
        .recheck(context.set(), context.now_millis())?;
    Ok(())
}
fn native_admission<'a>(
    bundle: &'a wire::NativePublicProofBundleV1,
    record: &wire::SignedRecord,
) -> Result<&'a host::SignedHostedWitnessStatementV1> {
    for signed in &bundle.statements {
        let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
        match s.purpose {
            1 => {
                for p in &bundle.genesis_witnesses {
                    if p.original_genesis.as_ref() == Some(record)
                        && hybrid_codec::canonical(p)? == s.canonical_payload
                    {
                        api::native_witness::verify_genesis_payload(s, p)?;
                        return Ok(signed);
                    }
                }
            }
            2 => {
                for p in &bundle.authority_witnesses {
                    if p.original.as_ref() == Some(record)
                        && hybrid_codec::canonical(p)? == s.canonical_payload
                    {
                        contract::verify_witness_payload(s, WitnessPayload::Authority(p))?;
                        return Ok(signed);
                    }
                }
            }
            4 => {
                for p in &bundle.landing_witnesses {
                    if p.execution.as_ref() == Some(record)
                        && hybrid_codec::canonical(p)? == s.canonical_payload
                    {
                        contract::verify_witness_payload(s, WitnessPayload::Landing(p))?;
                        return Ok(signed);
                    }
                }
            }
            _ => return Err(Reject::Version.into()),
        }
    }
    Err(Reject::Scope.into())
}
fn imported_admission<'a>(
    bundle: &'a wire::ImportPublicProofBundleV1,
    record: &wire::SignedRecord,
    genesis: &native::ThreadGenesis,
    context: &TrustTransaction<'_>,
) -> Result<(
    Option<native::delegated_import::DelegatedImport>,
    &'a host::HostedWitnessStatementV1,
)> {
    let (id, thread, _) = delegated_import::native_subject(record)?;
    if id != thread && record.format == native::OPERATION_FORMAT {
        let op = verify::verify_native_operation(record)?.1;
        let frontier = contract::frontier_digest(&wire::ImportFrontierV1 {
            format_version: 1,
            thread_id: thread.as_bytes().to_vec(),
            operation_ids: vec![id.as_bytes().to_vec()],
        })?;
        // Operations are progressive-manifest order. The first matching P3 is
        // the deterministic endpoint; later publication cannot relabel it.
        if let Some(operation) = bundle.operations.iter().find(|o| {
            o.body
                .as_ref()
                .is_some_and(|b| b.resulting_frontier_digest == frontier)
        }) {
            let body = operation.body.as_ref().ok_or(Reject::Canonical)?;
            let signed = bundle
                .delegations
                .iter()
                .find(|d| {
                    contract::signed_delegation_digest(d)
                        .is_ok_and(|digest| digest == body.delegation_digest)
                })
                .ok_or(Reject::Scope)?;
            let d = signed.body.as_ref().ok_or(Reject::Scope)?;
            let identity = d.identity.as_ref().ok_or(Reject::Scope)?;
            let (_, statement) = delegated_import::find_publication(bundle, operation)?;
            recheck_admission(context, id, statement, &bundle.history_proofs)?;
            let observation = statement.body.as_ref().ok_or(Reject::Canonical)?;
            let owners = delegated_import::public_owners(
                bundle.into(),
                observation.observed_at_unix_millis / 1000,
            )?;
            let hash: [u8; 32] = identity
                .owner_state_hash
                .as_slice()
                .try_into()
                .map_err(|_| Reject::Canonical)?;
            let owner = owners.get(&hash).ok_or(Reject::Scope)?;
            if identity.owner_id != owner.owner_id() {
                return Err(Reject::Root.into());
            }
            let member = contract::resolve_bundle_permission(bundle, &d.parent_permission_digest)?;
            let delegation = contract::verify_delegation(
                signed,
                member,
                &contract::ImportOwnerExpectation {
                    identity,
                    owner_public_key: &owner.authority_key().public_key,
                    owner_chain_digest: &d.owner_chain_digest,
                    authority_expires_at_seconds: owner.authority_expires_at_seconds(),
                    now_unix_seconds: observation.observed_at_unix_millis / 1000,
                    forbidden_job_keys: &context.forbidden_job_keys(),
                    known_job_associations: context.job_associations(),
                },
            )?;
            let parents = op
                .parents
                .iter()
                .map(|parent| {
                    installed_operation(context, *parent, thread)
                        .and_then(|r| Ok(verify::verify_native_operation(&r)?.1))
                })
                .collect::<Result<Vec<_>>>()?;
            let bound =
                verify::bind_import_original(bundle, &[&delegation], genesis, &op, &parents)?
                    .ok_or(Reject::ImportPermission)?;
            if bound.converted().publisher.as_slice() != d.job_public_key {
                return Err(Reject::ImportPermission.into());
            }
            return Ok((Some(bound), observation));
        }
    }
    for signed in &bundle.statements {
        let s = signed.body.as_ref().ok_or(Reject::Canonical)?;
        let matched = match s.purpose {
            1 => bundle
                .genesis_witnesses
                .iter()
                .find(|p| {
                    p.original_genesis.as_ref() == Some(record)
                        && hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                })
                .map(WitnessPayload::Genesis),
            2 => bundle
                .authority_witnesses
                .iter()
                .find(|p| {
                    p.original.as_ref() == Some(record)
                        && hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                })
                .map(WitnessPayload::Authority),
            4 => bundle
                .landing_witnesses
                .iter()
                .find(|p| {
                    p.execution.as_ref() == Some(record)
                        && hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload)
                })
                .map(WitnessPayload::Landing),
            _ => None,
        };
        if let Some(payload) = matched {
            contract::verify_witness_payload(s, payload)?;
            recheck_admission(context, id, signed, &bundle.history_proofs)?;
            return Ok((None, s));
        }
    }
    Err(Reject::Scope.into())
}

/// Require durable exact bytes, independently of any retained proof declaration.
pub(super) fn require_installed(
    context: &TrustTransaction<'_>,
    record: &wire::SignedRecord,
    id: ContentHash,
    thread: ContentHash,
) -> Result<()> {
    let installed = if id == thread {
        installed_genesis(context, thread)?.0
    } else {
        installed_subject(context, id, thread, &record.format)?
    };
    if installed != *record {
        return Err(Reject::Scope.into());
    }
    Ok(())
}

// Re-run the ordinary installer verifier on this endpoint's deterministic
// prefix, under the same mutation lock, with every persistence path disabled.
// Selection is anchored to the dependent's independently verified Spool lineage.
#[allow(clippy::too_many_arguments)]
fn recheck_prefix(
    proof: super::authority::PublicProof,
    original: &wire::SignedRecord,
    reference: &wire::ForeignDependencyV1,
    dependent_statements: &[host::SignedHostedWitnessStatementV1],
    dependent_authority: &[wire::ImportAuthorityWitnessV1],
    dependent_landing: &[wire::HostedLandingWitnessV1],
    spool_genesis: &[u8; 32],
    directory: &Path,
    store: &impl objects::store::ObjectStore,
    authority: &impl delegated_import::AcceptedAuthority,
    context: &TrustTransaction<'_>,
    check: &Recheck<'_>,
) -> Result<()> {
    use super::authority::{AcceptedHistory, PublicEvidence, PublicProof, SelectedAuthority};
    let commitment = reference.encode_to_vec();
    if check.verified.borrow().contains(&commitment) {
        return Ok(());
    }
    check
        .budget
        .borrow_mut()
        .visit(check.path.len() + 1, true)?;
    let original = original.clone();
    let proof = proof.prefix(reference)?;
    let dependent = dependent_statements
        .iter()
        .find_map(|signed| {
            let s = signed.body.as_ref()?;
            refers_to(s, &original, dependent_authority, dependent_landing)
                .ok()?
                .then_some(s)
        })
        .ok_or(Reject::Scope)?;
    let pinned = authority.for_witness(dependent)?;
    context.require_spool_selection(&pinned)?;
    let history = AcceptedHistory::from_public(
        &proof,
        pinned.keyring,
        context.now_millis() / 1000,
        pinned.limits,
    )
    .map_err(|e| super::Error::Invalid(e.to_string()))?;
    if history.genesis() != spool_genesis {
        return Err(Reject::Scope.into());
    }
    let mut path = check.path.to_vec();
    path.push(reference.clone());
    let next = Recheck {
        path: &path,
        verified: check.verified,
        budget: check.budget,
    };
    let retained = SelectedAuthority::from_proof(
        history,
        proof.clone(),
        retained_disclosure as fn(&PublicProof, i64, &TrustTransaction<'_>) -> Result<()>,
    );
    // The imported converted original may live only in the durable operation
    // journal; native local work likewise has no P2 sidecar.
    let (id, thread, _) = delegated_import::native_subject(&original)?;
    require_installed(context, &original, id, thread)?;
    proof.validate()?;
    let requested = prefix_causal_originals(&proof, &original, context)?;
    match proof {
        PublicProof::Native(bundle) => {
            super::native_witness::install_in(
                directory,
                &bundle,
                &requested,
                &retained,
                store,
                context,
                Some(&next),
            )?;
        }
        PublicProof::Import(bundle) => {
            delegated_import::install_in(
                directory,
                &bundle,
                &requested,
                &retained,
                store,
                context,
                Some(&next),
            )?;
        }
    }
    check.verified.borrow_mut().insert(commitment);
    Ok(())
}

fn retained_disclosure(
    _: &super::authority::PublicProof,
    _: i64,
    _: &TrustTransaction<'_>,
) -> Result<()> {
    // Retained rechecks admit no new work or disclosure.
    Ok(())
}

// Transfer operation frames are separate from the carrier. Recover exact own-
// origin causal frames from this receiver's journal for a read-only replay.
fn prefix_causal_originals(
    proof: &super::authority::PublicProof,
    original: &wire::SignedRecord,
    context: &TrustTransaction<'_>,
) -> Result<Vec<wire::SignedRecord>> {
    use super::authority::PublicProof;
    let (mut records, authority, landing) = match proof {
        PublicProof::Native(b) => (
            b.genesis_witnesses
                .iter()
                .filter_map(|p| p.original_genesis.clone())
                .collect::<Vec<_>>(),
            &b.authority_witnesses,
            &b.landing_witnesses,
        ),
        PublicProof::Import(b) => (
            b.original_geneses.clone(),
            &b.authority_witnesses,
            &b.landing_witnesses,
        ),
    };
    let own_threads = records
        .iter()
        .map(|r| Ok(verify::verify_native_genesis(r)?.1.id()?))
        .collect::<Result<BTreeSet<_>>>()?;
    records.extend(
        authority
            .iter()
            .flat_map(|p| p.original.iter().chain(&p.dependencies))
            .cloned(),
    );
    records.extend(
        landing
            .iter()
            .flat_map(|p| {
                p.execution
                    .iter()
                    .chain(p.source_operation.iter())
                    .chain(&p.review_evidence)
            })
            .cloned(),
    );
    records.push(original.clone());
    let mut known = BTreeSet::new();
    for r in &records {
        if let Ok((id, _, _)) = delegated_import::native_subject(r) {
            known.insert(id);
        }
    }
    let mut requested = vec![original.clone()];
    let mut index = 0;
    while index < records.len() {
        let record = &records[index];
        index += 1;
        let Ok((_, thread, dependencies)) = delegated_import::native_subject(record) else {
            continue;
        };
        if !own_threads.contains(&thread) {
            continue;
        }
        for id in dependencies {
            if !known.insert(id) {
                continue;
            }
            let row: Option<(Vec<u8>, String)> = context.sql().query_row(
                "SELECT thread,'heddle-thread-operation-v1' FROM operations WHERE id=?1 AND status=1 UNION ALL SELECT thread,'heddle-thread-ownership-claim-v1' FROM thread_owner_claims WHERE id=?1 UNION ALL SELECT thread,'heddle-thread-ownership-resolution-v1' FROM thread_owner_resolutions WHERE id=?1",
                [id.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)),
            ).optional()?;
            let (thread, format) = row.ok_or(Reject::Scope)?;
            let thread =
                ContentHash::from_bytes(thread.as_slice().try_into().map_err(|_| Reject::Scope)?);
            if !own_threads.contains(&thread) {
                return Err(Reject::Scope.into());
            }
            let record = installed_subject(context, id, thread, &format)?;
            requested.push(record.clone());
            records.push(record);
        }
    }
    Ok(requested)
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn foreign_prefix_budget_bounds_depth_and_total_sibling_work() {
        let mut budget = ForeignPrefixBudget::default();
        for depth in 1..=ForeignPrefixBudget::MAX_DEPTH {
            budget.visit(depth, false).expect("bounded chain");
        }
        assert!(matches!(
            budget.visit(ForeignPrefixBudget::MAX_DEPTH + 1, false),
            Err(super::super::Error::ForeignPrefixLimitExceeded {
                limit_name: "depth",
                ..
            })
        ));
        // A depth refusal does not spend the count budget. Shallow siblings
        // share what remains instead of each starting an unbounded traversal.
        for _ in ForeignPrefixBudget::MAX_DEPTH..ForeignPrefixBudget::MAX_PREFIXES {
            budget.visit(1, false).expect("remaining siblings");
        }
        assert!(matches!(
            budget.visit(1, false),
            Err(super::super::Error::ForeignPrefixLimitExceeded {
                limit_name: "count",
                ..
            })
        ));
    }
}
