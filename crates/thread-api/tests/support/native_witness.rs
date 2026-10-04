//! Test Weft witnesses the actual creator originals; it never signs their authority.
use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec as codec,
};
use crypto::{Ed25519Signer, Signer};
use objects::object::thread_replication::{
    GenesisOwner, SourceAuthor, ThreadGenesis, ThreadOperation,
};

pub(crate) fn ownership(spool: uuid::Uuid) -> (wire::SignedSpoolOwnerGenesis, wire::OwnerState) {
    ownership_for(spool, &["acme".into(), "widgets".into()])
}
pub(crate) fn ownership_for(
    spool: uuid::Uuid,
    path: &[String],
) -> (wire::SignedSpoolOwnerGenesis, wire::OwnerState) {
    let owner = Ed25519Signer::from_seed(&[71; 32]).expect("owner");
    let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery");
    let account = uuid::Uuid::from_u128(2);
    let root = repo::sign_custodial_owner_root(&owner, &recovery, *account.as_bytes(), [98; 32])
        .expect("root");
    let binding = repo::sign_custodial_owner_binding(&owner, &root, [99; 32]).expect("binding");
    let genesis = repo::sign_spool_owner_genesis(&owner, *spool.as_bytes()).expect("Spool genesis");
    let state = wire::OwnerState {
        owner: Some(wire::PrincipalRef {
            id: account.to_string(),
        }),
        root: Some(root.clone()),
        binding: Some(binding.clone()),
        version: binding.root_state_hash.clone(),
        resource_keyring: Some(wire::CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: spool.as_bytes().to_vec(),
            canonical_spool_path_segments: path.to_vec(),
            pin: Some(wire::CloneOwnerPin {
                kind: 2,
                expected_owner_id: root.root.as_ref().expect("root").owner_id.clone(),
                first_seen_unix_seconds: 1,
            }),
            owner_root: Some(root),
            accepted_state_hash: binding.root_state_hash,
            owner_genesis: Some(genesis.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    (genesis, state)
}
pub(crate) fn envelope(owner: &wire::OwnerState, token: &biscuit_auth::Biscuit) -> Vec<u8> {
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("owner");
    let authority = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![],
        revoked_ids: vec![],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };
    repo::thread_replication::metadata::prepare_control_authority(
        &authority,
        &signer.public_key().try_into().expect("key"),
        token,
        chrono::Utc::now().timestamp(),
    )
    .expect("native creator capability")
}
pub(crate) fn witness_set() -> host::SignedHostedWitnessSetV1 {
    witness_set_for("https://weft.example.test", "descriptor-root-1")
}
pub(crate) fn witness_set_for(authority: &str, root_id: &str) -> host::SignedHostedWitnessSetV1 {
    let witness = Ed25519Signer::from_seed(&[5; 32]).expect("witness");
    let now = chrono::Utc::now().timestamp_millis();
    let body = host::HostedWitnessSetV1 {
        format_version: 1,
        deployment_authority: authority.into(),
        descriptor_root_id: root_id.into(),
        generation: 1,
        issued_at_unix_millis: now - 1000,
        valid_until_unix_millis: now + 240_000,
        current_executor_id: api::witness_trust::witness_id(witness.public_key()),
        entries: vec![host::HostedWitnessEntryV1 {
            executor_id: api::witness_trust::witness_id(witness.public_key()),
            public_key: witness.public_key().to_vec(),
            role: 1,
            state: 1,
            purposes: vec![1, 2, 3, 4],
            active_from_unix_millis: 0,
            active_until_unix_millis: now + 300_000,
            ..Default::default()
        }],
    };
    let bytes = api::witness_trust::set_signing_bytes(&body).expect("set bytes");
    host::SignedHostedWitnessSetV1 {
        body: Some(body),
        body_digest: codec::hash(&[&bytes]),
        root_signature: Ed25519Signer::from_seed(&[7; 32])
            .expect("deployment root")
            .sign(&bytes)
            .expect("root signature"),
    }
}
fn signature_digest(records: &[&wire::SignedRecord]) -> Vec<u8> {
    let signatures = records
        .iter()
        .flat_map(|r| &r.signatures)
        .collect::<Vec<_>>();
    let mut bytes = (signatures.len() as u32).to_be_bytes().to_vec();
    for signature in signatures {
        bytes.extend(codec::canonical(signature).expect("signature"));
    }
    codec::hash(&[b"heddle-hosted-original-signatures-v1", &bytes])
}
pub(crate) fn bundle(
    owner_genesis: &wire::SignedSpoolOwnerGenesis,
    owner: &wire::OwnerState,
    genesis: &wire::ThreadGenesisRecord,
    operations: &[wire::ReplicationOperations],
    set: &host::SignedHostedWitnessSetV1,
    previous: Option<wire::NativePublicProofBundleV1>,
) -> wire::NativePublicProofBundleV1 {
    let binding = genesis
        .native_genesis_authority
        .as_ref()
        .expect("creator binding before request PoP");
    let original_genesis = genesis.genesis.as_ref().expect("original genesis");
    api::native_witness::verify_genesis_authority(
        binding,
        original_genesis,
        &genesis.creator_authority,
    )
    .expect("exact creator binding");
    let selected = binding
        .body
        .as_ref()
        .expect("body")
        .identity
        .as_ref()
        .expect("identity");
    let policy = {
        use heddleco_capability_verifier::policy;
        let mut body = wire::SignedPolicyBody {
            format_version: 1,
            spool_uuid: selected.spool_uuid.clone(),
            expected_head: Some(policy::zero_head()),
            sequence: 1,
            policy: Some(wire::SignedSpoolPolicy::default()),
            merge_policies: policy::required_merge_policies(),
            owner_id: selected.owner_id.clone(),
            owner_state_hash: selected.owner_state_hash.clone(),
            ownership_transfer_sequence: selected.ownership_transfer_sequence,
            ..Default::default()
        };
        body.policy_state_hash = policy::policy_state_hash(&body)
            .expect("policy state")
            .to_vec();
        let signer = Ed25519Signer::from_seed(&[71; 32]).expect("client owner");
        wire::SignedSpoolPolicyRecord {
            owner_signature: Some(wire::AuthorizationSignature {
                signer_key_id: codec::key_id(signer.public_key()),
                signature: signer
                    .sign(&policy::policy_signature_digest(&body).expect("policy signature bytes"))
                    .expect("client policy signature"),
            }),
            body: Some(body),
        }
    };
    let policy_hash = policy
        .body
        .as_ref()
        .expect("policy")
        .policy_state_hash
        .clone();
    let mut bundle = previous.unwrap_or_else(|| wire::NativePublicProofBundleV1 {
        format_version: 1,
        owner_genesis: Some(owner_genesis.clone()),
        owner_histories: vec![wire::OwnerHistory {
            root: owner.root.clone(),
            accepted_transitions: owner.accepted_transitions.clone(),
            state_hash: owner.version.clone(),
        }],
        owner_chains: vec![wire::ImportOwnerChainV1 {
            spool_genesis_digest: selected.spool_genesis_digest.clone(),
            owner_state_hashes: vec![owner.version.clone()],
            transfer_audit_hashes: vec![],
        }],
        witness_set: Some(set.clone()),
        policies: vec![policy],
        ..Default::default()
    });
    let sign = |purpose,
                canonical_payload,
                authority_digest,
                original_signatures_digest,
                publisher: &[u8],
                order| {
        let witness = Ed25519Signer::from_seed(&[5; 32]).expect("witness");
        let statement = host::HostedWitnessStatementV1 {
            format_version: 1,
            executor_id: api::witness_trust::witness_id(witness.public_key()),
            purpose,
            spool_uuid: selected.spool_uuid.clone(),
            spool_genesis_digest: selected.spool_genesis_digest.clone(),
            owner_id: selected.owner_id.clone(),
            owner_state_hash: selected.owner_state_hash.clone(),
            ownership_transfer_sequence: selected.ownership_transfer_sequence,
            policy_state_hash: policy_hash.clone(),
            policy_sequence: 1,
            basis: 1,
            publisher_key_id: codec::key_id(publisher),
            authority_digest,
            original_signatures_digest,
            host_transaction_id: vec![17; 16],
            admission_order: order,
            observed_at_unix_millis: chrono::Utc::now().timestamp_millis(),
            canonical_payload,
            boundary_acceptance: None,
        };
        let signature = witness
            .sign(&api::witness_trust::statement_signing_digest(&statement).expect("statement"))
            .expect("witness signature");
        host::SignedHostedWitnessStatementV1 {
            body: Some(statement),
            signature,
        }
    };
    if bundle.genesis_witnesses.is_empty() {
        let payload = wire::NativeGenesisWitnessV1 {
            format_version: 1,
            kind: 2,
            binding: Some(binding.clone()),
            original_genesis: Some(original_genesis.clone()),
            creator_authority_envelope: genesis.creator_authority.clone(),
            boundary_acceptance: None,
        };
        bundle.statements.push(sign(
            1,
            codec::canonical(&payload).expect("payload"),
            api::native_witness::signed_genesis_digest(binding).expect("binding"),
            signature_digest(&[original_genesis]),
            &binding.body.as_ref().expect("body").creator_public_key,
            1,
        ));
        bundle.genesis_witnesses.push(payload);
    }
    let all = operations
        .iter()
        .flat_map(|b| &b.operations)
        .collect::<Vec<_>>();
    for original in all
        .iter()
        .copied()
        .chain(&genesis.ownership_claims)
        .chain(&genesis.ownership_resolutions)
    {
        if bundle
            .authority_witnesses
            .iter()
            .any(|p| p.original.as_ref() == Some(original))
        {
            continue;
        }
        let (kind, envelope, publisher) = match original.format.as_str() {
            "heddle-thread-operation-v1" => {
                let operation =
                    ThreadOperation::decode(&original.canonical_record).expect("operation");
                match operation.source_author().expect("source author") {
                    Some(SourceAuthor::Account { authority, .. }) => {
                        (1, authority.clone(), operation.publisher.to_vec())
                    }
                    Some(SourceAuthor::LocalKey) => continue,
                    _ => panic!("fixture expects source capture"),
                }
            }
            "heddle-thread-ownership-claim-v1" => {
                let claim = objects::object::thread_replication::ownership_claim::ThreadOwnershipClaim::decode(&original.canonical_record).expect("claim");
                let SourceAuthor::Account { authority, .. } = claim.acceptance else {
                    panic!("account claim required");
                };
                (2, authority, claim.accepting_publisher.to_vec())
            }
            _ => panic!("fixture does not issue resolution or landing witnesses"),
        };
        let mut dependencies = all
            .iter()
            .filter(|r| *r != &original)
            .map(|r| (*r).clone())
            .chain(std::iter::once(original_genesis.clone()))
            .collect::<Vec<_>>();
        dependencies.sort_by_key(|r| {
            api::import_authority::signed_native_digest(r).expect("dependency digest")
        });
        let payload = wire::ImportAuthorityWitnessV1 {
            format_version: 1,
            kind,
            original: Some(original.clone()),
            authority_envelope: envelope.clone(),
            dependencies,
            boundary_acceptances: vec![],
        };
        let records = std::iter::once(original)
            .chain(&payload.dependencies)
            .collect::<Vec<_>>();
        let authority_digest = codec::hash(&[
            b"heddle-hosted-authority-envelope-v1",
            &(envelope.len() as u32).to_be_bytes(),
            &envelope,
        ]);
        bundle.statements.push(sign(
            2,
            codec::canonical(&payload).expect("payload"),
            authority_digest,
            signature_digest(&records),
            &publisher,
            bundle.statements.len() as u64 + 1,
        ));
        bundle.authority_witnesses.push(payload);
    }
    bundle.authority_witnesses.sort_by_key(|p| {
        codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("payload digest")
    });
    bundle.statements.sort_by_key(|s| {
        api::witness_trust::statement_signing_digest(s.body.as_ref().expect("statement"))
            .expect("digest")
    });
    let local = ThreadGenesis::decode(&original_genesis.canonical_record).expect("genesis");
    if matches!(local.owner, GenesisOwner::LocalKey(_)) {
        assert!(
            !genesis.ownership_claims.is_empty(),
            "explicit LocalKey ownership claim"
        );
    }
    api::native_witness::validate_public_bundle(&bundle).expect("complete native witness contract");
    bundle
}
