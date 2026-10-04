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
    json!({"id":name,"api":"native-genesis","binding_hex":hex::encode(p.binding.clone().unwrap_or_default().encode_to_vec()),"original_hex":hex::encode(p.original_genesis.as_ref().expect("original").encode_to_vec()),"envelope_hex":hex::encode(&p.creator_authority_envelope),"keyring_hex":hex::encode(keyring.encode_to_vec()),"current_owner_hex":hex::encode(observed.encode_to_vec()),"initial_owner_hex":hex::encode(owner.owner_id()),"spool_genesis_hex":hex::encode(digest),"revoked_keys_json":"[]","revoked_credentials_json":"[]","now":"1100","max_ttl":"3600","expected_accept":true})
}
fn verify(c: &Value) -> crate::Result<Vec<u8>> {
    let bytes = |field: &str| hex::decode(c[field].as_str().expect("hex field")).expect("hex");
    super::native_genesis::verify_bytes(
        &bytes("binding_hex"),
        &bytes("original_hex"),
        &bytes("envelope_hex"),
        &bytes("keyring_hex"),
        &bytes("current_owner_hex"),
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
fn native_creator_binding_checks_lineage_envelope_time_and_revocation() {
    let control = input("start_thread");
    verify(&control).expect("creator-bound original account authority");
    verify(&input("local_adopt_push"))
        .expect("local creator with exact empty envelope; hosted claim is separate");
    let mut cases = vec![control.clone(), input("local_adopt_push")];
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
