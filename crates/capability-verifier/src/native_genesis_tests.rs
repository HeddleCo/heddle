use heddle_api::{heddle::api::v1alpha2 as wire, hybrid_codec};
use prost::Message;
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/native-host-witness-v1.json"
    ))
    .expect("published vectors")
}
fn record<T: Message + Default>(f: &Value, name: &str) -> T {
    hybrid_codec::strict_decode(
        &hex::decode(f["wire_vectors"][name]["wire_hex"].as_str().expect("wire")).expect("hex"),
        heddle_api::import_authority::MAX_BUNDLE_BYTES,
    )
    .expect("canonical wire")
}
fn input(name: &str) -> Value {
    let f = fixture();
    let bundle: wire::NativePublicProofBundleV1 = record(&f, name);
    let history = &bundle.owner_histories[0];
    let owner = crate::verify_owner_root(history.root.as_ref().expect("root"))
        .expect("independent fixture owner");
    let genesis = bundle.owner_genesis.as_ref().expect("Spool");
    let digest = crate::creation::spool_genesis_digest(genesis.genesis.as_ref().expect("body"))
        .expect("lineage");
    let keyring = wire::CloneAuthorizationKeyring {
        format_version: 1,
        spool_uuid: genesis.genesis.as_ref().expect("body").spool_uuid.clone(),
        canonical_spool_path_segments: vec!["acme".into(), "imports".into()],
        owner_genesis: Some(genesis.clone()),
        owner_root: history.root.clone(),
        accepted_transitions: history.accepted_transitions.clone(),
        accepted_state_hash: history.state_hash.clone(),
        pin: Some(wire::CloneOwnerPin {
            kind: 2,
            expected_owner_id: owner.owner_id().to_vec(),
            first_seen_unix_seconds: 1000,
        }),
        ..Default::default()
    };
    let observed = wire::OwnerState {
        root: history.root.clone(),
        accepted_transitions: history.accepted_transitions.clone(),
        version: history.state_hash.clone(),
        ..Default::default()
    };
    let p = &bundle.genesis_witnesses[0];
    json!({"id":name,"api":"native-genesis","binding_hex":hex::encode(p.binding.clone().unwrap_or_default().encode_to_vec()),"original_hex":hex::encode(p.original_genesis.as_ref().expect("original").encode_to_vec()),"envelope_hex":hex::encode(&p.creator_authority_envelope),"keyring_hex":hex::encode(keyring.encode_to_vec()),"current_owner_hex":hex::encode(observed.encode_to_vec()),"author_history_hex":hex::encode(history.encode_to_vec()),"admitted_mint_roots_json":"[]","initial_owner_hex":hex::encode(owner.owner_id()),"spool_genesis_hex":hex::encode(digest),"revoked_keys_json":"[]","revoked_credentials_json":"[]","now":"1100","max_ttl":"3600","expected_accept":true})
}
fn verify(c: &Value) -> crate::Result<super::native_genesis::NativeGenesisSummary> {
    let bytes = |field: &str| hex::decode(c[field].as_str().expect("hex field")).expect("hex");
    super::native_genesis::verify_bytes(
        &bytes("binding_hex"),
        &bytes("original_hex"),
        &bytes("envelope_hex"),
        &bytes("keyring_hex"),
        &bytes("current_owner_hex"),
        &bytes("author_history_hex"),
        c["admitted_mint_roots_json"].as_str().expect("inventory"),
        &bytes("initial_owner_hex"),
        &bytes("spool_genesis_hex"),
        c["revoked_keys_json"].as_str().expect("keys"),
        c["revoked_credentials_json"].as_str().expect("credentials"),
        c["now"].as_str().expect("now").parse().expect("seconds"),
        3600,
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn native_creator_binding_commits_exact_envelope() {
    let control = input("start_thread");
    let bytes =
        |field: &str| hex::decode(control[field].as_str().expect("hex field")).expect("hex");
    let limits = crate::VerificationLimits::new(3600).expect("limits");
    let (keyring, owner) = crate::observed::ownership(
        &bytes("keyring_hex"),
        &bytes("current_owner_hex"),
        1100,
        limits,
    )
    .expect("independently selected ownership");
    let initial = bytes("initial_owner_hex").try_into().expect("owner id");
    let genesis = bytes("spool_genesis_hex").try_into().expect("Spool id");
    let selection = crate::import_delegation::Selection {
        owner: &owner,
        keyring: &keyring,
        initial_owner_id: &initial,
        spool_genesis_digest: &genesis,
        limits,
    };
    let binding = wire::SignedNativeGenesisAuthorityV1::decode(bytes("binding_hex").as_slice())
        .expect("binding");
    let original = wire::SignedRecord::decode(bytes("original_hex").as_slice()).expect("original");
    let verify = |envelope: &[u8]| {
        super::native_genesis::verify_binding(&binding, &original, envelope, &selection)
    };
    verify(&bytes("envelope_hex")).expect("exact creator envelope");
    let alternative: wire::NativePublicProofBundleV1 = record(&fixture(), "distinct_owner_chains");
    heddle_api::native_witness::validate_public_bundle(&alternative)
        .expect("another valid creator-bound history");
    let original_envelope = bytes("envelope_hex");
    let swapped = &alternative
        .genesis_witnesses
        .iter()
        .find(|p| {
            !p.creator_authority_envelope.is_empty()
                && p.creator_authority_envelope != original_envelope
        })
        .expect("different envelope from a passing native vector")
        .creator_authority_envelope;
    let envelope: wire::ThreadControlAuthority =
        hybrid_codec::strict_decode(swapped, heddle_api::import_authority::MAX_RECORD_BYTES)
            .expect("canonical alternative envelope");
    assert_eq!(envelope.format, 1);
    assert!(verify(swapped).is_err(), "substituted envelope must reject");
    verify(&bytes("envelope_hex")).expect("unchanged envelope passing control");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn native_creator_binding_checks_lineage_envelope_time_and_revocation() {
    let control = input("start_thread");
    verify(&control).expect("creator-bound original account authority");
    verify(&input("local_adopt_push"))
        .expect("local creator with exact empty envelope; hosted claim is separate");
    let mut cases = vec![control.clone(), input("local_adopt_push")];
    cases.extend(recovered_owner_cases());
    let envelope: wire::ThreadControlAuthority = hybrid_codec::strict_decode(
        &hex::decode(control["envelope_hex"].as_str().expect("envelope")).expect("hex"),
        65536,
    )
    .expect("canonical envelope");
    let wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(attachment) =
        envelope.mint_root_association.expect("paired creator")
    else {
        panic!("owner attachment");
    };
    let exact = hex::encode(attachment.encode_to_vec());
    let mut admitted = control.clone();
    admitted["id"] = json!("exact-authenticated-inventory");
    admitted["admitted_mint_roots_json"] =
        json!(serde_json::to_string(&[&exact]).expect("inventory"));
    verify(&admitted).expect("exact verified attachment");
    cases.push(admitted.clone());
    let mut forged = attachment;
    forged.attachment.as_mut().expect("body").nonce[0] ^= 1;
    for (label, roots) in [
        ("duplicate-inventory", vec![exact.clone(), exact.clone()]),
        (
            "unrelated-forged-inventory",
            vec![exact, hex::encode(forged.encode_to_vec())],
        ),
    ] {
        let mut denied = admitted.clone();
        denied["id"] = json!(label);
        denied["admitted_mint_roots_json"] =
            json!(serde_json::to_string(&roots).expect("inventory"));
        denied["expected_accept"] = json!(false);
        assert!(
            verify(&denied).is_err(),
            "{label}: every inventory entry must be independently valid"
        );
        cases.push(denied);
        verify(&admitted).expect("authenticated inventory control");
    }
    for name in ["missing_binding", "forged_binding", "substituted_envelope"] {
        let mut denied = input(name);
        denied["expected_accept"] = json!(false);
        assert!(verify(&denied).is_err(), "{name}");
        cases.push(denied);
        verify(&control).expect("nearby passing control");
    }
    for field in ["initial_owner_hex", "spool_genesis_hex"] {
        let mut denied = control.clone();
        denied["id"] = json!(format!("wrong-{field}"));
        denied[field] = json!("00".repeat(32));
        denied["expected_accept"] = json!(false);
        assert!(verify(&denied).is_err(), "{field}");
        cases.push(denied);
        verify(&control).expect("independent lineage control");
    }
    for (label, field, value) in [
        ("expired", "now", "100000".to_owned()),
        (
            "revoked-creator",
            "revoked_keys_json",
            serde_json::to_string(&[hex::encode(hybrid_codec::key_id(
                &hex::decode(
                    fixture()["keys"]["device"]["public_key_hex"]
                        .as_str()
                        .expect("creator"),
                )
                .expect("key"),
            ))])
            .expect("keys"),
        ),
        (
            "revoked-session",
            "revoked_credentials_json",
            "[\"hybrid-native-fixture\"]".to_owned(),
        ),
    ] {
        let mut denied = control.clone();
        denied["id"] = json!(label);
        denied[field] = json!(value);
        denied["expected_accept"] = json!(false);
        assert!(verify(&denied).is_err(), "{label}");
        cases.push(denied);
        verify(&control).expect("nearby unrevoked in-window control");
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::fs::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("conformance/fixtures/native-genesis-v1.json"),
        serde_json::to_string_pretty(&json!({"cases":cases})).expect("JSON"),
    )
    .expect("parity corpus");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn native_summary_distinguishes_account_authority_from_local_binding() {
    for (name, kind, requires_claim) in [
        ("start_thread", "account", false),
        ("local_adopt_push", "local_key", true),
    ] {
        let outcome =
            serde_json::to_value(verify(&input(name)).expect("verified binding")).expect("summary");
        assert_eq!(
            outcome["owner_kind"],
            json!(kind),
            "binding kind must be explicit"
        );
        assert_eq!(outcome["requires_hosting_claim"], json!(requires_claim));
    }
}

// Signed controls are rebuilt from published seeds; no signed bytes are edited.
fn recovered_owner_cases() -> Vec<Value> {
    use ed25519_dalek::{Signer, SigningKey};
    use heddle_biscuit_verifier::signature_v1::BiscuitBuilderV1Ext;
    let f = fixture();
    let key = |role: &str| {
        if role == "next_guardian_a" {
            return SigningKey::from_bytes(&[91; 32]);
        }
        if role == "next_guardian_b" {
            return SigningKey::from_bytes(&[92; 32]);
        }
        SigningKey::from_bytes(
            &hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed"))
                .expect("seed bytes")
                .try_into()
                .expect("32 bytes"),
        )
    };
    let sign = |role: &str, digest: &[u8]| wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(key(role).verifying_key().as_bytes()),
        signature: key(role).sign(digest).to_bytes().to_vec(),
    };
    let mut denied = input("start_thread");
    let decode = |field: &str| hex::decode(denied[field].as_str().expect("wire")).expect("hex");
    let mut history =
        wire::OwnerHistory::decode(decode("author_history_hex").as_slice()).expect("history");
    let initial = crate::creation::history_state(&history, 1100).expect("initial owner");
    let mut next_policy = initial.recovery_policy().clone();
    for (guardian, role) in next_policy
        .guardians
        .iter_mut()
        .zip(["next_guardian_a", "next_guardian_b"])
    {
        guardian.key.as_mut().expect("key").public_key =
            key(role).verifying_key().to_bytes().to_vec();
    }
    next_policy
        .guardians
        .sort_by_key(|g| hybrid_codec::key_id(&g.key.as_ref().expect("key").public_key));
    let body = wire::OwnerKeyTransition {
        format_version: 1,
        owner_id: initial.owner_id().to_vec(),
        previous_state_hash: initial.state_hash().to_vec(),
        sequence: 1,
        kind: wire::OwnerKeyTransitionKind::Recover as i32,
        next_authority_key: Some(wire::AuthorizationVerificationKey {
            algorithm: wire::AuthorizationKeyAlgorithm::Ed25519 as i32,
            public_key: key("rotated_owner").verifying_key().to_bytes().to_vec(),
        }),
        next_recovery_policy: Some(next_policy),
        valid_from_unix_seconds: 1090,
        previous_key_valid_until_unix_seconds: 0,
        nonce: vec![99; 32],
    };
    let digest = crate::canonical::digest(
        crate::canonical::OWNER_TRANSITION_DOMAIN,
        &crate::canonical::transition_body(&body).expect("Recover body"),
    );
    let mut guardians = vec![sign("guardian_a", &digest), sign("guardian_b", &digest)];
    guardians.sort_by(|a, b| a.signer_key_id.cmp(&b.signer_key_id));
    let mut next_guardians = vec![
        sign("next_guardian_a", &digest),
        sign("next_guardian_b", &digest),
    ];
    next_guardians.sort_by(|a, b| a.signer_key_id.cmp(&b.signer_key_id));
    let transition = wire::SignedOwnerKeyTransition {
        transition: Some(body),
        authorizations: guardians.clone(),
        next_authority_key_proof: Some(sign("rotated_owner", &digest)),
        next_recovery_key_proofs: next_guardians,
    };
    let limits = crate::VerificationLimits::new(3600).expect("limits");
    let current =
        crate::apply_accepted_transition(&initial, &transition, 1100, limits).expect("Recover");
    history.accepted_transitions.push(transition);
    history.state_hash = current.state_hash().to_vec();
    let mut ring =
        wire::CloneAuthorizationKeyring::decode(decode("keyring_hex").as_slice()).expect("ring");
    ring.accepted_transitions = history.accepted_transitions.clone();
    ring.accepted_state_hash = history.state_hash.clone();
    let mut observed =
        wire::OwnerState::decode(decode("current_owner_hex").as_slice()).expect("observation");
    observed.accepted_transitions = history.accepted_transitions.clone();
    observed.version = history.state_hash.clone();
    let mut binding =
        wire::SignedNativeGenesisAuthorityV1::decode(decode("binding_hex").as_slice())
            .expect("binding");
    let envelope =
        wire::ThreadControlAuthority::decode(decode("envelope_hex").as_slice()).expect("envelope");
    let wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(attachment) =
        envelope
            .mint_root_association
            .clone()
            .expect("device attachment")
    else {
        panic!("attachment")
    };
    // The current lineage binding is genuine even though the device author
    // attempts to carry only its older, independently valid account history.
    let verified_ring =
        crate::verify_clone_keyring(ring.clone(), 1100, limits, &[]).expect("current ring");
    let genesis = decode("spool_genesis_hex")
        .try_into()
        .expect("genesis digest");
    let initial_id = initial.owner_id();
    let (identity, chain, _) =
        crate::import_delegation::native_lineage(&crate::import_delegation::Selection {
            owner: &current,
            keyring: &verified_ring,
            spool_genesis_digest: &genesis,
            initial_owner_id: &initial_id,
            limits,
        })
        .expect("current lineage");
    let b = binding.body.as_mut().expect("body");
    b.identity = Some(identity);
    b.owner_chain_digest = chain;
    binding.creator_signature = Some(sign(
        "device",
        &hybrid_codec::signing_digest(heddle_api::native_witness::GENESIS_DOMAIN, b)
            .expect("binding digest"),
    ));
    denied["id"] = json!("truncated-pre-recover-cut-device");
    denied["keyring_hex"] = json!(hex::encode(ring.encode_to_vec()));
    denied["current_owner_hex"] = json!(hex::encode(observed.encode_to_vec()));
    denied["binding_hex"] = json!(hex::encode(binding.encode_to_vec()));
    denied["admitted_mint_roots_json"] = json!(
        serde_json::to_string(&[hex::encode(attachment.encode_to_vec())]).expect("inventory")
    );
    denied["expected_accept"] = json!(false);

    // Fresh authority from the recovered owner, same account and publisher.
    let mut control = denied.clone();
    let mut envelope = envelope;
    let publisher = hex::encode(key("device").verifying_key().to_bytes());
    let seed = key("rotated_owner").to_bytes();
    let pair = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&seed, biscuit_auth::Algorithm::Ed25519)
            .expect("mint"),
    );
    let next = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&[94; 32], biscuit_auth::Algorithm::Ed25519)
            .expect("deterministic next key"),
    );
    let token = biscuit_auth::Biscuit::builder().code(format!(
        "user(\"11111111-1111-1111-1111-111111111111\"); session(\"recovered-owner\"); device_pop_key(\"{publisher}\"); check if operation(\"StartThread\"); check if resource(\"spool\", \"acme/imports\"); check if time($now), $now < 1970-01-01T00:30:00Z;"
    )).expect("facts").build_v1_with_key_pair(&pair, &next).expect("capability");
    envelope.owner = Some(history.clone());
    envelope.mint_root_public_key = current.authority_key().public_key.clone();
    envelope.mint_root_association = None;
    envelope.sealed_biscuit = token.seal().expect("seal").to_vec().expect("sealed bytes");
    let bytes = envelope.encode_to_vec();
    let b = binding.body.as_mut().expect("body");
    b.creator_authority_envelope_digest = hybrid_codec::hash(&[&bytes]);
    binding.creator_signature = Some(sign(
        "device",
        &hybrid_codec::signing_digest(heddle_api::native_witness::GENESIS_DOMAIN, b)
            .expect("binding digest"),
    ));
    control["id"] = json!("current-recovered-owner-history");
    control["author_history_hex"] = json!(hex::encode(history.encode_to_vec()));
    control["envelope_hex"] = json!(hex::encode(bytes));
    control["binding_hex"] = json!(hex::encode(binding.encode_to_vec()));
    control["admitted_mint_roots_json"] = json!("[]");
    control["expected_accept"] = json!(true);
    vec![denied, control]
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn native_genesis_owner_pin_cuts_truncated_pre_recover_author_history() {
    let cases = recovered_owner_cases();
    assert_eq!(
        verify(&cases[0]),
        Err(crate::Error::BrokenChain(
            "attachment issuer is unknown or recovered".into()
        )),
        "WASM owner pin must reject the truncated history's cut device"
    );
    verify(&cases[1]).expect("current recovered owner history control");
}
