//! Shared inputs for the native and public JS import-delegation routes.
use ed25519_dalek::{Signer, SigningKey};
use heddle_api::{hybrid_codec, import_authority as contract};
use heddleco_capability_verifier::{import_delegation, wire::*};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Inputs {
    certificate_hex: String,
    permission_hex: String,
    keyring_hex: String,
    owner_history_hex: String,
    initial_owner_hex: String,
    spool_genesis_hex: String,
    forbidden_json: String,
    associations_json: String,
    cancellations_json: String,
    revoked_json: String,
    now: String,
    max_ttl: String,
}
pub fn evaluate(json: &str) -> Result<Value, String> {
    let c: Inputs = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let bytes = |s: &str| hex::decode(s).map_err(|e| e.to_string());
    import_delegation::verify_bytes(
        &bytes(&c.certificate_hex)?,
        &bytes(&c.permission_hex)?,
        &bytes(&c.keyring_hex)?,
        &bytes(&c.owner_history_hex)?,
        &bytes(&c.initial_owner_hex)?,
        &bytes(&c.spool_genesis_hex)?,
        &c.forbidden_json,
        &c.associations_json,
        &c.cancellations_json,
        &c.revoked_json,
        c.now.parse::<i64>().map_err(|_| "invalid owner-authorization object: now_unix_seconds is outside the i64 range")?,
        c.max_ttl.parse::<i64>().map_err(|_| "invalid owner-authorization object: max_capability_ttl_seconds is outside the i64 range")?,
    )
    .map(|digest| json!(hex::encode(digest)))
    .map_err(|e| e.to_string())
}
fn record<T: Message + Default>(f: &Value, name: &str) -> Result<T, String> {
    let v = f["signed_vectors"]
        .get(name)
        .or_else(|| f["wire_vectors"].get(name))
        .ok_or("missing vector")?;
    let bytes = hex::decode(v["wire_hex"].as_str().ok_or("missing wire bytes")?)
        .map_err(|e| e.to_string())?;
    hybrid_codec::strict_decode(&bytes, contract::MAX_BUNDLE_BYTES).map_err(|e| e.to_string())
}
fn key(f: &Value, role: &str) -> Result<Vec<u8>, String> {
    hex::decode(f["keys"][role]["public_key_hex"].as_str().ok_or("key")?).map_err(|e| e.to_string())
}
fn sign<T: hybrid_codec::Canonical>(
    f: &Value,
    role: &str,
    domain: &str,
    body: &T,
) -> Result<AuthorizationSignature, String> {
    let seed: [u8; 32] = hex::decode(f["keys"][role]["seed_hex"].as_str().ok_or("seed")?)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "seed width")?;
    Ok(AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(&key(f, role)?),
        signature: SigningKey::from_bytes(&seed)
            .sign(&hybrid_codec::signing_digest(domain, body).map_err(|e| e.to_string())?)
            .to_bytes()
            .to_vec(),
    })
}
pub fn cases(f: &Value) -> Result<Value, String> {
    let h: OwnerHistory = record(f, "owner_history")?;
    let owner = heddleco_capability_verifier::verify_owner_root(h.root.as_ref().ok_or("root")?)
        .map_err(|e| e.to_string())?;
    let g: SignedSpoolOwnerGenesis = record(f, "spool_owner_genesis")?;
    let digest = heddleco_capability_verifier::creation::spool_genesis_digest(
        g.genesis.as_ref().ok_or("genesis")?,
    )
    .map_err(|e| e.to_string())?;
    let ring = CloneAuthorizationKeyring {
        format_version: 1,
        spool_uuid: g.genesis.as_ref().ok_or("body")?.spool_uuid.clone(),
        owner_genesis: Some(g),
        owner_root: h.root.clone(),
        canonical_spool_path_segments: vec!["example".into()],
        accepted_state_hash: owner.state_hash().to_vec(),
        pin: Some(CloneOwnerPin {
            kind: 2,
            expected_owner_id: owner.owner_id().to_vec(),
            first_seen_unix_seconds: 1000,
        }),
        ..Default::default()
    };
    let d: SignedImportJobDelegationV1 = record(f, "delegation")?;
    let p: SignedImportMemberPermissionV1 = record(f, "permission")?;
    let base = Inputs {
        certificate_hex: hex::encode(d.encode_to_vec()),
        permission_hex: hex::encode(p.encode_to_vec()),
        keyring_hex: hex::encode(ring.encode_to_vec()),
        owner_history_hex: hex::encode(h.encode_to_vec()),
        initial_owner_hex: hex::encode(owner.owner_id()),
        spool_genesis_hex: hex::encode(digest),
        forbidden_json: serde_json::to_string(&[
            hex::encode(key(f, "root")?),
            hex::encode(key(f, "witness")?),
            hex::encode(key(f, "next_witness")?),
        ])
        .map_err(|e| e.to_string())?,
        associations_json: "[]".into(),
        cancellations_json: "[]".into(),
        revoked_json: "[]".into(),
        now: "1100".into(),
        max_ttl: "3600".into(),
    };
    let mut out = Vec::new();
    let mut add = |id: &str, c: Inputs, expected: Option<&str>| -> Result<(), String> {
        out.push(json!({"id":format!("import-{id}"),"fixture_kind":"import","fixture_json":serde_json::to_string(&c).map_err(|e|e.to_string())?,"expected_error":expected}));
        Ok(())
    };
    add("device-control", base.clone(), None)?;
    for now in [999, 1300, 1400] {
        let mut c = base.clone();
        c.now = now.to_string();
        add(
            &format!("expiry-{now}"),
            c,
            Some("authority is not valid at verification time"),
        )?;
    }
    for role in ["witness", "job"] {
        let mut parent = p.clone();
        let b = parent.body.as_mut().ok_or("permission")?;
        b.subject_public_key = key(f, role)?;
        parent.owner_signature = Some(sign(f, "owner", contract::PERMISSION_DOMAIN, b)?);
        let mut child = d.clone();
        let b = child.body.as_mut().ok_or("delegation")?;
        b.delegating_public_key = key(f, role)?;
        b.job_public_key = key(f, "renew_job")?;
        b.job_key_id = hybrid_codec::key_id(&b.job_public_key);
        b.parent_permission_digest =
            contract::signed_permission_digest(&parent).map_err(|e| e.to_string())?;
        child.delegating_signature = Some(sign(f, role, contract::DELEGATION_DOMAIN, b)?);
        let mut c = base.clone();
        c.certificate_hex = hex::encode(child.encode_to_vec());
        c.permission_hex = hex::encode(parent.encode_to_vec());
        if role == "job" {
            c.associations_json = serde_json::to_string(&[(
                hex::encode(key(f, "job")?),
                hex::encode(d.body.as_ref().ok_or("body")?.logical_job_id.clone()),
            )])
            .map_err(|e| e.to_string())?;
        }
        add(&format!("{role}-delegator"), c, Some("key roles overlap"))?;
    }
    let mut direct = d.clone();
    let b = direct.body.as_mut().ok_or("body")?;
    b.delegating_public_key = key(f, "owner")?;
    b.parent_permission_digest = vec![0; 32];
    direct.delegating_signature = Some(sign(f, "owner", contract::DELEGATION_DOMAIN, b)?);
    let mut c = base.clone();
    c.certificate_hex = hex::encode(direct.encode_to_vec());
    c.permission_hex.clear();
    add("direct-owner-control", c, None)?;
    for role in ["owner", "device", "job"] {
        let mut c = base.clone();
        c.revoked_json =
            serde_json::to_string(&[hex::encode(hybrid_codec::key_id(&key(f, role)?))])
                .map_err(|e| e.to_string())?;
        add(&format!("revoked-{role}"), c, Some("witness is revoked"))?;
    }
    let mut c = base.clone();
    c.cancellations_json = serde_json::to_string(&[hex::encode(
        d.body.as_ref().ok_or("body")?.cancellation_id.clone(),
    )])
    .map_err(|e| e.to_string())?;
    add("cancelled", c, Some("witness is revoked"))?;
    let mut c = base.clone();
    c.permission_hex.clear();
    add(
        "missing-parent",
        c,
        Some("missing typed owner import permission"),
    )?;
    let mut c = base.clone();
    c.spool_genesis_hex = hex::encode([42; 32]);
    add(
        "wrong-spool",
        c,
        Some("independent authority/root mismatch"),
    )?;
    let mut c = base.clone();
    c.now = i64::MAX.to_string();
    add("time-overflow", c, Some("size or count bound exceeded"))?;
    let mut c = base;
    c.forbidden_json = "[\"00\"]".into();
    add(
        "json-key-width",
        c,
        Some("invalid owner-authorization object: wrong identifier width"),
    )?;
    let claimed: Value = serde_json::from_str(include_str!(
        "../../../conformance/hybrid/claimed-owner-expiry-v1.json"
    ))
    .map_err(|e| e.to_string())?;
    for case in claimed["cases"].as_array().ok_or("claimed-owner cases")? {
        let c = Inputs {
            certificate_hex: case["certificate_hex"]
                .as_str()
                .ok_or("certificate")?
                .into(),
            permission_hex: String::new(),
            keyring_hex: claimed["keyring_hex"].as_str().ok_or("keyring")?.into(),
            owner_history_hex: case["owner_history_hex"].as_str().ok_or("history")?.into(),
            initial_owner_hex: claimed["initial_owner_hex"]
                .as_str()
                .ok_or("initial owner")?
                .into(),
            spool_genesis_hex: claimed["spool_genesis_hex"]
                .as_str()
                .ok_or("genesis")?
                .into(),
            forbidden_json: "[]".into(),
            associations_json: "[]".into(),
            cancellations_json: "[]".into(),
            revoked_json: "[]".into(),
            now: case["now"].as_str().ok_or("now")?.into(),
            max_ttl: "3600".into(),
        };
        add(
            case["id"].as_str().ok_or("case id")?,
            c.clone(),
            case["expected_error"].as_str(),
        )?;
        if case["id"] == "claimed-human-after-deadline" {
            let mut revoked = c;
            revoked.revoked_json =
                serde_json::to_string(&[hex::encode(hybrid_codec::key_id(&key(f, "device")?))])
                    .map_err(|e| e.to_string())?;
            add("claimed-human-revoked", revoked, Some("witness is revoked"))?;
        }
    }
    let mut large = d.clone();
    let large_body = large.body.as_mut().ok_or("body")?;
    large_body.scope.as_mut().ok_or("scope")?.max_result_bytes = u64::MAX;
    let mut parent = p.clone();
    let parent_body = parent.body.as_mut().ok_or("parent")?;
    parent_body.scope.as_mut().ok_or("scope")?.max_result_bytes = u64::MAX;
    parent.owner_signature = Some(sign(f, "owner", contract::PERMISSION_DOMAIN, parent_body)?);
    large_body.parent_permission_digest =
        contract::signed_permission_digest(&parent).map_err(|e| e.to_string())?;
    large.delegating_signature = Some(sign(f, "device", contract::DELEGATION_DOMAIN, large_body)?);
    let mut c = Inputs {
        certificate_hex: hex::encode(large.encode_to_vec()),
        permission_hex: hex::encode(parent.encode_to_vec()),
        keyring_hex: hex::encode(ring.encode_to_vec()),
        owner_history_hex: hex::encode(h.encode_to_vec()),
        initial_owner_hex: hex::encode(owner.owner_id()),
        spool_genesis_hex: hex::encode(digest),
        forbidden_json: "[]".into(),
        associations_json: "[]".into(),
        cancellations_json: "[]".into(),
        revoked_json: "[]".into(),
        now: "1100".into(),
        max_ttl: "3600".into(),
    };
    add("u64-max-total", c.clone(), None)?;
    let signed: SignedImportJobDelegationV1 = record(f, "alpha32_large_owner")?;
    c.certificate_hex = hex::encode(signed.encode_to_vec());
    c.permission_hex.clear();
    add("large-owner-total", c, None)?;
    for (id, scope, manifest) in [
        (
            "empty",
            record::<ImportPermissionScopeV1>(f, "scope")?,
            record::<ImportResultManifestV1>(f, "empty_manifest")?,
        ),
        (
            "partial",
            record(f, "scope")?,
            record(f, "partial_manifest")?,
        ),
        (
            "complete",
            record(f, "scope")?,
            record(f, "terminal_manifest")?,
        ),
        (
            "removed-not-recharged",
            record(f, "alpha32_remaining")?,
            record(f, "partial_manifest")?,
        ),
    ] {
        out.push(json!({"id":format!("import-scope-{id}"), "fixture_kind":"production", "fixture_json":json!({
            "api":"import-scope", "scope_hex":hex::encode(scope.encode_to_vec()),
            "manifest_hex":hex::encode(manifest.encode_to_vec()), "now":"1100", "expected_accept":true,
        }).to_string()}));
    }
    for (id, request, manifest) in [
        ("large", "alpha32_commit_large", "alpha32_manifest_large"),
        (
            "u64-max",
            "alpha32_commit_u64_max",
            "alpha32_manifest_u64_max",
        ),
    ] {
        let request: CommitImportJobRequest = record(f, request)?;
        let scope = request
            .proof
            .as_ref()
            .ok_or("proof")?
            .delegations
            .last()
            .ok_or("delegation")?
            .body
            .as_ref()
            .ok_or("body")?
            .scope
            .as_ref()
            .ok_or("scope")?;
        let manifest: ImportResultManifestV1 = record(f, manifest)?;
        out.push(json!({"id":format!("import-scope-{id}"), "fixture_kind":"production", "fixture_json":json!({
            "api":"import-scope", "scope_hex":hex::encode(scope.encode_to_vec()),
            "manifest_hex":hex::encode(manifest.encode_to_vec()), "now":"1100", "expected_accept":true,
        }).to_string()}));
    }
    Ok(Value::Array(out))
}
