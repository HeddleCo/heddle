//! Signed controls that reach the native installer gates, rather than failing
//! earlier portable signature or reference verification.
use biscuit_verifier::signature_v1::BiscuitBuilderV1Ext;
use crypto::{Ed25519Signer, Signer};
use objects::object::thread_replication::{
    ownership_claim as claim, ownership_resolution as resolution,
};

use super::*;

fn fresh_spool_landing() -> wire::NativePublicProofBundleV1 {
    let mut bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "native_landing");
    bundle.policies.clear();
    for signed in &mut bundle.statements {
        let body = signed.body.as_mut().expect("statement");
        body.policy_sequence = 0;
        body.policy_state_hash = vec![0; 32];
        let entry = bundle
            .witness_set
            .as_ref()
            .expect("set")
            .body
            .as_ref()
            .expect("set body")
            .entries
            .iter()
            .find(|entry| entry.executor_id == body.executor_id)
            .expect("witness key");
        signed.signature = signer(&entry.public_key)
            .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
            .expect("witness signature");
    }
    sort(&mut bundle);
    bundle
}

#[test]
fn native_landing_accepts_genesis_governance_with_nonzero_review_policy() {
    let bundle = fresh_spool_landing();
    assert!(!bundle.landing_witnesses.is_empty());
    for payload in &bundle.landing_witnesses {
        let (_, execution) =
            verify::verify_native_operation(payload.execution.as_ref().expect("execution"))
                .expect("signed execution");
        assert_ne!(
            execution
                .integration()
                .expect("receipt")
                .expect("integration")
                .review_policy_version
                .as_bytes(),
            &[0; 32],
        );
    }
    api::native_witness::verify_bundle_witnesses(&bundle, &set(&bundle, 1_100_000), 1_100_000, &[])
        .expect("API authenticates genesis governance independently");
    verify_semantics(&bundle, 1_100_000)
        .expect("fresh Spool landing retains its separate review policy");
    let originals: Vec<_> = bundle
        .authority_witnesses
        .iter()
        .filter_map(|p| p.original.clone())
        .chain(
            bundle
                .landing_witnesses
                .iter()
                .filter_map(|p| p.execution.clone()),
        )
        .collect();
    local_work::install_bundle("fresh Spool landing", bundle, &originals, None);
}

fn change_landing_review_policy(
    bundle: &mut wire::NativePublicProofBundleV1,
    change_execution_policy: bool,
) {
    use objects::object::{ContentHash, thread_replication::ThreadOperationBody};

    let payload = bundle
        .landing_witnesses
        .iter_mut()
        .find(|p| !p.review_evidence.is_empty())
        .expect("landing with reviews");
    let old_payload = hybrid_codec::canonical(payload).expect("payload");
    let execution = payload.execution.as_mut().expect("execution");
    let (_, mut operation) = verify::verify_native_operation(execution).expect("operation");
    let mut integration = operation
        .integration()
        .expect("receipt")
        .expect("integration");
    let version = ContentHash::from_bytes([42; 32]);
    assert_ne!(integration.review_policy_version, version);
    // Request-only changes isolate its selection gate. Changing both the
    // request and receipt instead leaves only the original reviews mismatched.
    if change_execution_policy {
        integration.review_policy_version = version;
    }
    let request = payload.request.as_mut().expect("request");
    let mut body: wire::LandThreadRequest =
        hybrid_codec::strict_decode(&request.request_body, 65536).expect("request body");
    body.expected_policy_version = version.as_bytes().to_vec();
    request.request_body = body.encode_to_vec();
    let signature = request.signature.as_mut().expect("signature");
    let mut proof = api::signing::unary_bytes(
        &request.signing_identity,
        &request.method_path,
        request.timestamp_millis,
        &request.nonce,
        &request.request_body,
    );
    signature.signature = signer(&signature.public_key)
        .sign(&proof)
        .expect("request PoP");
    proof.extend_from_slice(&signature.signature);
    integration.initiating_request_proof =
        ContentHash::compute_typed("weft-hosted-landing-request-proof-v1", &proof);
    operation.body = ThreadOperationBody::Integration(integration.encode().expect("receipt"));
    let signed =
        crypto::thread_operation::SignedOperation::sign(&operation, &signer(&operation.publisher))
            .expect("execution signature");
    execution.canonical_record = signed.canonical;
    execution.signatures[0].signature = signed.signature;

    let statement = bundle
        .statements
        .iter_mut()
        .find(|s| s.body.as_ref().expect("body").canonical_payload == old_payload)
        .expect("landing statement");
    let body = statement.body.as_mut().expect("body");
    body.canonical_payload = hybrid_codec::canonical(payload).expect("changed payload");
    let signatures: Vec<_> = payload
        .execution
        .iter()
        .chain(&payload.source_operation)
        .chain(&payload.review_evidence)
        .flat_map(|r| &r.signatures)
        .chain(payload.request.as_ref().expect("request").signature.iter())
        .collect();
    let mut bytes = (signatures.len() as u32).to_be_bytes().to_vec();
    for signature in signatures {
        bytes.extend(hybrid_codec::canonical(signature).expect("signature"));
    }
    body.original_signatures_digest =
        hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &bytes]);
    statement.signature = signer(&operation.publisher)
        .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
        .expect("witness signature");
    bundle.landing_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-hosted-landing-witness-payload-v1", p).expect("digest")
    });
    sort(bundle);
}

#[test]
fn native_landing_rejects_signed_request_policy_mismatch() {
    let mut bundle = fresh_spool_landing();
    verify_semantics(&bundle, 1_100_000).expect("passing control");
    change_landing_review_policy(&mut bundle, false);
    api::native_witness::verify_bundle_witnesses(&bundle, &set(&bundle, 1_100_000), 1_100_000, &[])
        .expect("valid signatures, commitments and governance history");
    let error =
        verify_semantics(&bundle, 1_100_000).expect_err("request policy must match receipt");
    assert!(
        matches!(
            error.downcast_ref::<verify::Error>(),
            Some(verify::Error::Contract(hybrid_codec::Reject::Scope))
        ),
        "{error:?}"
    );
}

#[test]
fn native_landing_rejects_review_policy_mismatch() {
    let mut bundle = fresh_spool_landing();
    verify_semantics(&bundle, 1_100_000).expect("passing control");
    change_landing_review_policy(&mut bundle, true);
    api::native_witness::verify_bundle_witnesses(&bundle, &set(&bundle, 1_100_000), 1_100_000, &[])
        .expect("valid signatures, commitments and governance history");
    let error = verify_semantics(&bundle, 1_100_000).expect_err("review policy must match receipt");
    assert!(
        matches!(
            error.downcast_ref::<verify::Error>(),
            Some(verify::Error::Contract(hybrid_codec::Reject::Scope))
        ),
        "{error:?}"
    );
}

#[test]
fn native_genesis_policy_accepts_empty_revocations_on_fresh_receiver() {
    let mut bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "account_source");
    bundle.policies.clear();
    for signed in &mut bundle.statements {
        let body = signed.body.as_mut().expect("witness statement");
        body.policy_sequence = 0;
        body.policy_state_hash = vec![0; 32];
        let entry = bundle
            .witness_set
            .as_ref()
            .expect("set")
            .body
            .as_ref()
            .expect("set body")
            .entries
            .iter()
            .find(|entry| entry.executor_id == body.executor_id)
            .expect("witness key");
        signed.signature = signer(&entry.public_key)
            .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
            .expect("witness signature");
    }
    sort(&mut bundle);
    verify_semantics(&bundle, 1_100_000).expect("genesis policy has no native revocations");
    let originals = bundle
        .authority_witnesses
        .iter()
        .filter_map(|p| p.original.clone())
        .collect::<Vec<_>>();
    local_work::install_bundle("native genesis policy", bundle.clone(), &originals, None);
    for hash in [Vec::new(), vec![1; 32]] {
        let mut unknown = bundle.clone();
        for signed in &mut unknown.statements {
            let body = signed.body.as_mut().expect("statement");
            body.policy_state_hash = hash.clone();
            let entry = unknown
                .witness_set
                .as_ref()
                .expect("set")
                .body
                .as_ref()
                .expect("body")
                .entries
                .iter()
                .find(|entry| entry.executor_id == body.executor_id)
                .expect("key");
            signed.signature = signer(&entry.public_key)
                .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
                .expect("signature");
        }
        sort(&mut unknown);
        assert!(
            verify_semantics(&unknown, 1_100_000).is_err(),
            "unknown/absent native policy observation is not genesis"
        );
    }
}

fn genesis(bundle: &wire::NativePublicProofBundleV1) -> Vec<wire::SignedRecord> {
    bundle
        .genesis_witnesses
        .iter()
        .filter_map(|p| p.original_genesis.clone())
        .collect()
}
fn signer(key: &[u8]) -> Ed25519Signer {
    let f = fixture();
    let entry = f["keys"]
        .as_object()
        .expect("keys")
        .values()
        .find(|v| hex::decode(v["public_key_hex"].as_str().expect("key")).expect("hex") == key)
        .expect("fixture signer");
    Ed25519Signer::from_seed(&hex::decode(entry["seed_hex"].as_str().expect("seed")).expect("hex"))
        .expect("signer")
}
fn sort(bundle: &mut wire::NativePublicProofBundleV1) {
    bundle.authority_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("digest")
    });
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
}
fn replace_payload(
    bundle: &mut wire::NativePublicProofBundleV1,
    old: &wire::ImportAuthorityWitnessV1,
    new: wire::ImportAuthorityWitnessV1,
) {
    let canonical = hybrid_codec::canonical(old).expect("old payload");
    let statement = bundle
        .statements
        .iter_mut()
        .find(|s| s.body.as_ref().expect("body").canonical_payload == canonical)
        .expect("statement");
    let body = statement.body.as_mut().expect("body");
    body.canonical_payload = hybrid_codec::canonical(&new).expect("new payload");
    body.authority_digest = hybrid_codec::hash(&[
        b"heddle-hosted-authority-envelope-v1",
        &(new.authority_envelope.len() as u32).to_be_bytes(),
        &new.authority_envelope,
    ]);
    let signatures: Vec<_> = new
        .original
        .iter()
        .chain(&new.dependencies)
        .flat_map(|r| &r.signatures)
        .collect();
    let mut bytes = (signatures.len() as u32).to_be_bytes().to_vec();
    for signature in signatures {
        bytes.extend(hybrid_codec::canonical(signature).expect("signature"));
    }
    body.original_signatures_digest =
        hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &bytes]);
    let witness = fixture()["keys"]["witness"]["public_key_hex"]
        .as_str()
        .expect("witness")
        .to_owned();
    statement.signature = signer(&hex::decode(witness).expect("hex"))
        .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
        .expect("witness signature");
    *bundle
        .authority_witnesses
        .iter_mut()
        .find(|p| *p == old)
        .expect("payload") = new;
    sort(bundle);
}
fn remove_sidecars(
    bundle: &mut wire::NativePublicProofBundleV1,
    remove: impl Fn(&wire::ImportAuthorityWitnessV1) -> bool,
) {
    let removed: Vec<_> = bundle
        .authority_witnesses
        .iter()
        .filter(|p| remove(p))
        .map(|p| hybrid_codec::canonical(p).expect("payload"))
        .collect();
    bundle.authority_witnesses.retain(|p| !remove(p));
    bundle
        .statements
        .retain(|s| !removed.contains(&s.body.as_ref().expect("body").canonical_payload));
}

#[test]
fn native_requested_account_capture_requires_a_witness_payload() {
    let start: wire::NativePublicProofBundleV1 = record(&fixture(), "start_thread");
    let source: wire::NativePublicProofBundleV1 = record(&fixture(), "account_source");
    assert_eq!(genesis(&start), genesis(&source));
    let capture = source
        .authority_witnesses
        .iter()
        .find(|p| p.kind == 1)
        .and_then(|p| p.original.clone())
        .expect("genuine account capture");
    verify::verify_native_operation(&capture).expect("real device signature");
    verify_semantics(&start, 1_100_000).expect("genesis-only carrier");
    local_work::install_bundle(
        "genesis-only control",
        start.clone(),
        &genesis(&start),
        None,
    );
    local_work::install_bundle(
        "unwitnessed requested capture",
        start,
        std::slice::from_ref(&capture),
        Some(hybrid_codec::Reject::Scope),
    );
    local_work::install_bundle("witnessed capture control", source, &[capture], None);
}

#[test]
fn native_requested_claim_requires_its_purpose2_sidecar() {
    let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "local_adopt_push");
    let resolved: wire::NativePublicProofBundleV1 = record(&fixture(), "ownership_resolution");
    let missing = resolved
        .authority_witnesses
        .iter()
        .filter(|p| p.kind == 2)
        .filter_map(|p| p.original.clone())
        .find(|r| {
            claim::ThreadOwnershipClaim::decode(&r.canonical_record)
                .expect("claim")
                .source_frontier
                .is_empty()
        })
        .expect("genuine uncarried claim");
    local_work::install_bundle(
        "sole witnessed claim",
        bundle.clone(),
        &genesis(&bundle),
        None,
    );
    verify_semantics(&bundle, 1_100_000).expect("valid sole-claim carrier");
    local_work::install_bundle(
        "claim without purpose 2",
        bundle,
        &[missing],
        Some(hybrid_codec::Reject::Scope),
    );
}

/// Both claims cover the same source head. The ancestry walk therefore cannot
/// hide removal of the conflict decision gate.
fn same_frontier_conflict() -> wire::NativePublicProofBundleV1 {
    let mut bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "ownership_resolution");
    let claims: Vec<_> = bundle
        .authority_witnesses
        .iter()
        .filter(|p| p.kind == 2)
        .cloned()
        .collect();
    assert_eq!(claims.len(), 2);
    let first = claim::ThreadOwnershipClaim::decode(
        &claims[0].original.as_ref().expect("claim").canonical_record,
    )
    .expect("claim");
    let old = claims[1].original.as_ref().expect("claim");
    let mut second = claim::ThreadOwnershipClaim::decode(&old.canonical_record).expect("claim");
    let old_id = second.id().expect("id");
    second.source_frontier = first.source_frontier;
    let objects::object::thread_replication::SourceAuthor::Account {
        spool,
        actor,
        authority,
        ..
    } = &second.acceptance
    else {
        panic!("account");
    };
    let envelope: wire::ThreadControlAuthority =
        hybrid_codec::strict_decode(authority, 65536).expect("envelope");
    let f = fixture();
    let mint = f["keys"]
        .as_object()
        .expect("keys")
        .values()
        .find(|v| {
            hex::decode(v["public_key_hex"].as_str().expect("key")).expect("hex")
                == envelope.mint_root_public_key
        })
        .expect("mint seed");
    let key = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(
            &hex::decode(mint["seed_hex"].as_str().expect("seed")).expect("hex"),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("key"),
    );
    let token=biscuit_auth::Biscuit::builder().code(format!("user(\"{}\"); session(\"same-frontier\"); device_pop_key(\"{}\"); check if operation(\"ClaimThreadOwnership\"); check if resource(\"spool\", \"acme/imports\"); check if time($now), $now < 1970-01-01T01:00:00Z;",actor.principal_id,hex::encode(second.accepting_publisher))).expect("facts").build_v1(&key).expect("new portable capability");
    let authority = heddleco_capability_verifier::thread_control_authority::encode(
        envelope.owner.as_ref().expect("owner"),
        &envelope.mint_root_public_key,
        envelope.mint_root_association,
        &token,
    )
    .expect("envelope");
    second.acceptance = objects::object::thread_replication::SourceAuthor::account(
        *spool,
        actor.clone(),
        authority,
    )
    .expect("author");
    let signed = crypto::thread_ownership_claim::SignedOwnershipClaim::sign(
        &second,
        &signer(&second.prior_local_key),
        &signer(&second.accepting_publisher),
    )
    .expect("both signatures");
    let mut new = old.clone();
    new.canonical_record = signed.canonical;
    for sig in &mut new.signatures {
        sig.signature = if sig.public_key == second.prior_local_key {
            signed.local_signature.clone()
        } else {
            signed.acceptance_signature.clone()
        };
    }
    let payloads = bundle.authority_witnesses.clone();
    for payload in payloads {
        let mut changed = payload.clone();
        if changed.original.as_ref() == Some(old) {
            changed.original = Some(new.clone());
            if let objects::object::thread_replication::SourceAuthor::Account {
                authority, ..
            } = &second.acceptance
            {
                changed.authority_envelope = authority.clone();
            }
        }
        for dependency in &mut changed.dependencies {
            if dependency == old {
                *dependency = new.clone();
            }
        }
        if changed.kind == 3 {
            let original = changed.original.as_mut().expect("resolution");
            let mut r = resolution::ThreadOwnershipResolution::decode(&original.canonical_record)
                .expect("resolution");
            r.conflicting_claims.remove(&old_id);
            r.conflicting_claims.insert(second.id().expect("id"));
            if r.winning_claim == old_id {
                r.winning_claim = second.id().expect("id");
            }
            let signed = crypto::thread_ownership_resolution::SignedOwnershipResolution::sign(
                &r,
                &signer(&r.local_owner),
                &signer(&r.accepting_publisher),
            )
            .expect("both signatures");
            original.canonical_record = signed.canonical;
            for sig in &mut original.signatures {
                sig.signature = if sig.public_key == r.local_owner {
                    signed.local_signature.clone()
                } else {
                    signed.acceptance_signature.clone()
                };
            }
        }
        changed
            .dependencies
            .sort_by_key(|r| api::import_authority::signed_native_digest(r).expect("digest"));
        replace_payload(&mut bundle, &payload, changed);
    }
    verify_semantics(&bundle, 1_100_000).expect("same-frontier signed control");
    bundle
}
#[test]
fn native_conflicting_claims_need_a_resolution_even_with_identical_frontiers() {
    let mut bundle = same_frontier_conflict();
    local_work::install_bundle("resolved same-frontier control", bundle.clone(), &[], None);
    remove_sidecars(&mut bundle, |p| p.kind == 3);
    verify_semantics(&bundle, 1_100_000).expect("valid unresolved originals");
    local_work::install_bundle(
        "unresolved same-frontier claims",
        bundle,
        &[],
        Some(hybrid_codec::Reject::Scope),
    );
}

#[test]
fn native_binding_rejects_ambiguous_owner_chain_resolution() {
    use super::super::authority::tests::{bundle as imported, selected};
    let mut bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "start_thread");
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let pinned = selected(&imported(), limits);
    AcceptedHistory::from_native_spool(&bundle, &pinned, 1100, limits)
        .expect("single chain control");
    bundle
        .owner_histories
        .push(bundle.owner_histories[0].clone());
    assert_eq!(
        api::native_witness::validate_public_bundle(&bundle),
        Err(hybrid_codec::Reject::Canonical)
    );
    assert!(
        matches!(
            AcceptedHistory::from_native_spool(&bundle, &pinned, 1100, limits),
            Err(super::super::authority::Error::Rejected(
                hybrid_codec::Reject::Canonical
            ))
        ),
        "ambiguous owner chain must reject"
    );
}

#[test]
fn native_genesis_creator_cannot_be_a_job_key_or_authority_key() {
    let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "start_thread");
    verify_semantics(&bundle, 1_100_000).expect("ordinary native creator control");
    let creator = bundle.genesis_witnesses[0]
        .binding
        .as_ref()
        .expect("binding")
        .body
        .as_ref()
        .expect("body")
        .creator_public_key
        .clone();
    for (jobs, forbidden) in [
        (vec![(creator.clone(), vec![42; 16])], vec![]),
        (vec![], vec![creator]),
    ] {
        let error = verify_semantics_with_keys(&bundle, 1_100_000, &jobs, &forbidden)
            .expect_err("native creator key role must reject");
        assert!(
            matches!(
                error.downcast_ref::<crypto::import_authority::Error>(),
                Some(crypto::import_authority::Error::Contract(
                    hybrid_codec::Reject::KeyRole
                ))
            ),
            "{error:?}"
        );
    }
    verify_semantics(&bundle, 1_100_000).expect("unchanged creator control");
}

#[tokio::test]
async fn native_hosted_export_refuses_a_carrierless_local_original() {
    use std::sync::Arc;

    use objects::object::{
        Attribution, Principal, State, Tree,
        thread_replication::{
            AuthoredCapture, GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    };
    use repo::thread_replication::{ThreadReplica, hosted_trust::*};

    use super::super::authority::tests::{bundle as imported, selected};
    use crate::replication::{
        native::{Error as ReplicaError, LocalReplica},
        store::ReplicaStore,
    };
    let directory = tempfile::tempdir().expect("receiver");
    let repository = repo::Repository::init_default(directory.path()).expect("repository");
    let signer = Ed25519Signer::from_seed(&[31; 32]).expect("local key");
    let key = signer.public_key().try_into().expect("key");
    let base = repository.head().expect("head").expect("base");
    let g = ThreadGenesis {
        version: 1,
        spool: uuid::Uuid::from_u128(22).to_string(),
        owner: GenesisOwner::LocalKey(key),
        creator: key,
        parent: None,
        base,
        name: "local".into(),
        intent: String::new(),
        nonce: vec![1],
    };
    let replica = ThreadReplica::create(
        repository.heddle_dir(),
        &crypto::thread_operation::SignedGenesis::sign(&g, &signer).expect("genesis"),
    )
    .expect("replica");
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![base],
        Attribution::human(Principal::new("owner", "")),
    );
    let op = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: Default::default(),
        publisher: key,
        body: ThreadOperationBody::Capture(AuthoredCapture::local(
            replica
                .prepare_capture(&repository, &state)
                .expect("references"),
        )),
    };
    let signed = crypto::thread_operation::SignedOperation::sign(&op, &signer).expect("capture");
    replica
        .receive(&signed, repository.store(), |_| Ok(()))
        .expect("local receipt");
    let local = LocalReplica::new(replica, Arc::new(repository.store().clone()));
    let id = op.id().expect("id");
    assert!(
        local
            .operation(id)
            .await
            .expect("local export control")
            .is_some()
    );
    let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "start_thread");
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let pinned = selected(&imported(), limits);
    let history =
        AcceptedHistory::from_native_spool(&bundle, &pinned, 1100, limits).expect("authority");
    let authority = SelectedAuthority::new_native(
        history,
        bundle,
        |_: &wire::NativePublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let root = RootSelection {
        authority: "https://weft.example.test".into(),
        root_id: "descriptor-root-1".into(),
        public_key: hex::decode(
            fixture()["keys"]["root"]["public_key_hex"]
                .as_str()
                .expect("root"),
        )
        .expect("hex")
        .try_into()
        .expect("key"),
    };
    select_root(repository.heddle_dir(), &root).expect("root");
    let trust =
        HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock).expect("trust");
    let hosted = local.with_hosted_authority(
        repository.heddle_dir().to_owned(),
        Arc::new(trust),
        Arc::new(authority),
    );
    assert!(
        matches!(
            hosted.operation(id).await,
            Err(ReplicaError::HostedTrustRequired)
        ),
        "carrierless hosted export must reject"
    );
}

#[test]
fn selected_authority_zero_policy_record_refuses_native_revocations() {
    use heddleco_capability_verifier::{self as permission, VerificationLimits};
    use repo::thread_replication::hosted_trust::TrustTransaction;
    let mut clean: wire::NativePublicProofBundleV1 = record(&fixture(), "account_source");
    let mut zero = clean.policies[0].clone();
    clean.policies.clear();
    for signed in &mut clean.statements {
        let body = signed.body.as_mut().expect("statement");
        body.policy_sequence = 0;
        body.policy_state_hash = vec![0; 32];
        let entry = clean
            .witness_set
            .as_ref()
            .expect("set")
            .body
            .as_ref()
            .expect("body")
            .entries
            .iter()
            .find(|e| e.executor_id == body.executor_id)
            .expect("executor");
        signed.signature = signer(&entry.public_key)
            .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
            .expect("signature");
    }
    sort(&mut clean);
    let body = zero.body.as_mut().expect("policy");
    body.sequence = 0;
    body.policy_state_hash = vec![0; 32];
    let f = fixture();
    let owner = Ed25519Signer::from_seed(
        &hex::decode(f["keys"]["owner"]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("owner");
    zero.owner_signature = Some(wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(owner.public_key()),
        signature: owner
            .sign(&permission::policy::policy_signature_digest(body).expect("digest"))
            .expect("signature"),
    });
    let statement = clean
        .statements
        .iter()
        .find_map(|s| s.body.as_ref().filter(|s| s.purpose == 1))
        .expect("genesis");
    let original = clean
        .genesis_witnesses
        .iter()
        .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == statement.canonical_payload))
        .expect("original")
        .original_genesis
        .as_ref()
        .expect("genesis");
    let publisher = &original.signatures[0].public_key;
    let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let pinned =
        super::super::authority::tests::selected(&super::super::authority::tests::bundle(), limits);
    for poisoned in [false, true] {
        // Build accepted history before injecting the zero record. The assertion
        // calls the local predicate without portable policy-chain validation.
        let history =
            AcceptedHistory::from_native_spool(&clean, &pinned, 1100, limits).expect("history");
        let mut carried = clean.clone();
        if poisoned {
            carried.policies.push(zero.clone());
        }
        let authority = SelectedAuthority::new_native(
            history,
            carried,
            |_: &wire::NativePublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
        );
        let revoked = authority.native_revoked(
            statement,
            permission::thread_control_authority::Revocation::Publisher(publisher),
        );
        assert_eq!(
            revoked, poisoned,
            "local native genesis guard must reject a signed zero record"
        );
        for (sequence, hash) in [(0, Vec::new()), (0, vec![1; 32]), (1, vec![1; 32])] {
            let mut unknown = statement.clone();
            unknown.policy_sequence = sequence;
            unknown.policy_state_hash = hash;
            assert!(
                authority.native_revoked(
                    &unknown,
                    permission::thread_control_authority::Revocation::Publisher(publisher)
                ),
                "unknown or absent native policy fails closed directly"
            );
        }
    }
}
