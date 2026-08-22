use std::{net::Ipv4Addr, sync::Arc};

use api::{
    framing::{ResponseFrame, decode_response_frame, encode_request_frame},
    heddle::api::v1alpha1::{CallContext, CallFailure, CallFailureCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crypto::{Ed25519Signer, Signer as _};
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};
use tokio::sync::Mutex;

use super::{
    claim_authorization::{
        AgentConsent, consent_reply, pre_consent_message, promote_consent_message, resolved_reply,
        signed_pre_consent, signed_promote_consent, validate_credential_id, validate_handle,
        verify_promotion_consents,
    },
    hosted::claim_protocol::{
        CLAIM_ALPN_V1, CLAIM_CONSENT_METHOD, CLAIM_RESOLVE_METHOD, ClaimHandler, ClaimProtocol,
        ClaimSecretVerifier, VerifiedClaimPrincipal,
    },
    identity_state::ClaimState,
};

const OWNER_ID: &str = "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28";

fn state(node_id: String) -> ClaimState {
    ClaimState::new(
        "api.heddle.test".into(),
        uuid::Uuid::parse_str(OWNER_ID).unwrap(),
        "subject-7".into(),
        "steady-heron".into(),
        node_id,
    )
}

fn consent_from_reply(value: &serde_json::Value) -> AgentConsent {
    serde_json::from_value(value["consent"].clone()).expect("consent payload")
}

#[derive(Clone)]
struct MockWeftAccount {
    owner_id: uuid::Uuid,
    spool_owner_id: uuid::Uuid,
    root_credential_id: Option<String>,
}

struct MockPromotion<'a> {
    handle: &'a str,
    nonce: &'a [u8],
    credential_id: &'a str,
    pre: &'a AgentConsent,
    promote: &'a AgentConsent,
    now_millis: i64,
}

impl MockWeftAccount {
    fn promote(&mut self, agent_public_key: &[u8], promotion: &MockPromotion<'_>) -> bool {
        if self.root_credential_id.is_some() {
            return false;
        }
        if verify_promotion_consents(
            agent_public_key,
            promotion.handle,
            promotion.nonce,
            promotion.credential_id,
            promotion.pre,
            promotion.promote,
            promotion.now_millis,
        )
        .is_err()
        {
            return false;
        }
        self.root_credential_id = Some(promotion.credential_id.to_string());
        true
    }
}

#[test]
fn consent_signatures_round_trip_the_local_builders() {
    let signer = Ed25519Signer::generate().expect("signer");
    let mut claim = state(hex::encode(signer.public_key()));
    assert!(claim.reissue(b"claim-secret", i64::MAX));
    let nonce = b"0123456789abcdef";

    let pre =
        signed_pre_consent(&claim, "human-handle", nonce, &signer).expect("signed pre-consent");
    let pre_signature = URL_SAFE_NO_PAD.decode(pre.signature).unwrap();
    Ed25519Signer::verify_with_public_key(
        &pre_consent_message(&claim, "human-handle", nonce).unwrap(),
        signer.public_key(),
        &pre_signature,
    )
    .expect("local pre-consent verifies");

    let promote = signed_promote_consent(&claim, "human-handle", "Y3JlZGVudGlhbA", &signer)
        .expect("signed promote-consent");
    let promote_signature = URL_SAFE_NO_PAD.decode(promote.signature).unwrap();
    Ed25519Signer::verify_with_public_key(
        &promote_consent_message(&claim, "human-handle", "Y3JlZGVudGlhbA").unwrap(),
        signer.public_key(),
        &promote_signature,
    )
    .expect("local promote-consent verifies");

    assert_eq!(pre.account_id, OWNER_ID);
    assert_eq!(promote.account_id, OWNER_ID);
    assert_eq!(pre.authorization_hash, claim.authorization_hash());
    assert_eq!(promote.authorization_hash, claim.authorization_hash());
    assert_eq!(pre.expires_at, claim.expires_at_millis);
    assert_eq!(promote.expires_at, claim.expires_at_millis);
}

#[test]
fn consent_builders_encode_the_v1_counted_fields() {
    let mut claim = state("11".repeat(32));
    assert!(claim.reissue(b"claim-secret", 1_700_000_000_000));
    let nonce = b"0123456789abcdef";
    let pre = pre_consent_message(&claim, "human-handle", nonce).expect("pre-consent bytes");
    let promote = promote_consent_message(&claim, "human-handle", "Y3JlZGVudGlhbA")
        .expect("promote-consent bytes");

    assert_eq!(
        pre,
        counted(&[
            b"heddle-agent-pre-consent-v1",
            OWNER_ID.as_bytes(),
            b"human-handle",
            claim.node_id.as_bytes(),
            nonce,
            claim.authorization_hash().as_bytes(),
            &claim.expires_at_millis.to_be_bytes(),
        ])
    );
    assert_eq!(
        promote,
        counted(&[
            b"heddle-agent-promote-consent-v1",
            OWNER_ID.as_bytes(),
            b"human-handle",
            b"Y3JlZGVudGlhbA",
            claim.authorization_hash().as_bytes(),
            &claim.expires_at_millis.to_be_bytes(),
        ])
    );
}

#[test]
fn expired_consent_is_not_produced_or_accepted() {
    let signer = Ed25519Signer::generate().expect("signer");
    let mut claim = state(hex::encode(signer.public_key()));
    assert!(claim.reissue(b"claim-secret", chrono::Utc::now().timestamp_millis() - 1));
    let nonce = b"0123456789abcdef";

    assert!(
        signed_pre_consent(&claim, "human-handle", nonce, &signer).is_err(),
        "expired pre-consent must not be issued"
    );
    assert!(
        signed_promote_consent(&claim, "human-handle", "Y3JlZGVudGlhbA", &signer).is_err(),
        "expired promote-consent must not be issued"
    );

    let pre_bytes = pre_consent_message(&claim, "human-handle", nonce).expect("expired encoding");
    let promote_bytes = promote_consent_message(&claim, "human-handle", "Y3JlZGVudGlhbA")
        .expect("expired encoding");
    let pre = AgentConsent {
        account_id: claim.owner_id.to_string(),
        node_id: claim.node_id.clone(),
        signature: URL_SAFE_NO_PAD.encode(signer.sign(&pre_bytes).expect("sign stale pre-consent")),
        authorization_hash: claim.authorization_hash().to_string(),
        expires_at: claim.expires_at_millis,
    };
    let promote = AgentConsent {
        account_id: claim.owner_id.to_string(),
        node_id: claim.node_id.clone(),
        signature: URL_SAFE_NO_PAD.encode(
            signer
                .sign(&promote_bytes)
                .expect("sign stale promote-consent"),
        ),
        authorization_hash: claim.authorization_hash().to_string(),
        expires_at: claim.expires_at_millis,
    };
    let mut account = MockWeftAccount {
        owner_id: claim.owner_id,
        spool_owner_id: claim.owner_id,
        root_credential_id: None,
    };
    assert!(
        !account.promote(
            signer.public_key(),
            &MockPromotion {
                handle: "human-handle",
                nonce,
                credential_id: "Y3JlZGVudGlhbA",
                pre: &pre,
                promote: &promote,
                now_millis: chrono::Utc::now().timestamp_millis(),
            },
        ),
        "expired consent must not be accepted from the bound expiry"
    );
}

#[test]
fn expired_consent_signature_is_rejected_after_bound_expiry() {
    let signer = Ed25519Signer::generate().expect("signer");
    let mut claim = state(hex::encode(signer.public_key()));
    let expires_at = chrono::Utc::now().timestamp_millis() + 60_000;
    assert!(claim.reissue(b"claim-secret", expires_at));
    let nonce = b"0123456789abcdef";

    let pre =
        signed_pre_consent(&claim, "human-handle", nonce, &signer).expect("signed pre-consent");
    let promote = signed_promote_consent(&claim, "human-handle", "Y3JlZGVudGlhbA", &signer)
        .expect("signed promote-consent");

    verify_promotion_consents(
        signer.public_key(),
        "human-handle",
        nonce,
        "Y3JlZGVudGlhbA",
        &pre,
        &promote,
        expires_at - 1,
    )
    .expect("signature must verify before the bound expiry");

    let mut later_local_ttl = state(hex::encode(signer.public_key()));
    assert!(later_local_ttl.reissue(b"later-secret", expires_at + 60_000));
    assert!(
        later_local_ttl.consent_unexpired(expires_at),
        "local claim TTL remaining must not keep a stale signature alive"
    );
    assert!(
        verify_promotion_consents(
            signer.public_key(),
            "human-handle",
            nonce,
            "Y3JlZGVudGlhbA",
            &pre,
            &promote,
            expires_at,
        )
        .is_err(),
        "server-side verify must reject once now >= signed expiresAt"
    );

    let mut stretched = pre.clone();
    stretched.expires_at = expires_at + 60_000;
    assert!(
        verify_promotion_consents(
            signer.public_key(),
            "human-handle",
            nonce,
            "Y3JlZGVudGlhbA",
            &stretched,
            &promote,
            expires_at,
        )
        .is_err(),
        "extending expiresAt on the payload must not revive the signature"
    );
}

fn counted(parts: &[&[u8]]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for part in parts {
        let length = u32::try_from(part.len()).expect("test field fits in u32");
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(part);
    }
    encoded
}

#[test]
fn unbound_claim_state_cannot_encode_consent() {
    let claim = state("11".repeat(32));
    assert!(
        pre_consent_message(&claim, "human-handle", b"0123456789abcdef").is_err(),
        "dormant state has no claim-state id or expiry to bind"
    );
    assert!(
        promote_consent_message(&claim, "human-handle", "Y3JlZGVudGlhbA").is_err(),
        "dormant state has no claim-state id or expiry to bind"
    );
}

#[test]
fn malformed_claim_inputs_are_rejected() {
    assert!(validate_handle("UPPERCASE").is_err());
    assert!(validate_handle("-leading").is_err());
    assert!(validate_credential_id("not+base64url").is_err());
}

struct MemoryClaimAuthorization {
    state: Mutex<ClaimState>,
    secret: Vec<u8>,
    signer: Ed25519Signer,
}

impl std::fmt::Debug for MemoryClaimAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryClaimAuthorization")
            .finish_non_exhaustive()
    }
}

impl ClaimSecretVerifier for MemoryClaimAuthorization {
    async fn verify(
        &self,
        _method: &str,
        context: &CallContext,
        _body: &[u8],
    ) -> Result<VerifiedClaimPrincipal, CallFailure> {
        let state = self.state.lock().await;
        if !state.accepts(
            &context.bearer_capability,
            chrono::Utc::now().timestamp_millis(),
        ) {
            return Err(failure(
                CallFailureCode::Unauthenticated,
                "claim authorization failed",
            ));
        }
        Ok(VerifiedClaimPrincipal {
            subject: state.owner_id.to_string(),
            authorization_hash: state.authorization_hash().to_string(),
        })
    }
}

impl ClaimHandler for MemoryClaimAuthorization {
    async fn call(
        &self,
        method: &str,
        principal: VerifiedClaimPrincipal,
        body: &[u8],
    ) -> Result<Vec<u8>, CallFailure> {
        let mut state = self.state.lock().await;
        if principal.subject != state.owner_id.to_string()
            || principal.authorization_hash != state.authorization_hash()
            || self.secret.is_empty()
        {
            return Err(failure(
                CallFailureCode::Unauthenticated,
                "claim authorization failed",
            ));
        }
        match method {
            CLAIM_RESOLVE_METHOD => resolved_reply(&state, body),
            CLAIM_CONSENT_METHOD => consent_reply(&mut state, body, &self.signer),
            _ => Err(failure(CallFailureCode::Unimplemented, "unknown method")),
        }
    }
}

fn failure(code: CallFailureCode, message: &str) -> CallFailure {
    CallFailure {
        code: code as i32,
        message: message.to_string(),
        error: None,
    }
}

async fn endpoints(
    authorization: Arc<MemoryClaimAuthorization>,
) -> (Router, Endpoint, iroh::EndpointAddr) {
    let server = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let address = server.addr();
    let router = Router::builder(server)
        .accept(
            CLAIM_ALPN_V1,
            ClaimProtocol::new(Arc::clone(&authorization), authorization),
        )
        .spawn();
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    (router, client, address)
}

async fn call(
    client: &Endpoint,
    server: iroh::EndpointAddr,
    method: &str,
    secret: &[u8],
    body: &[u8],
) -> OwnedResponse {
    let connection = client.connect(server, CLAIM_ALPN_V1).await.unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    let frame = encode_request_frame(
        method,
        &CallContext {
            bearer_capability: secret.to_vec(),
            ..CallContext::default()
        },
        body,
    )
    .unwrap();
    send.write_all(&frame).await.unwrap();
    send.finish().unwrap();
    let response = recv.read_to_end(1024 * 1024).await.unwrap();
    match decode_response_frame(&response).unwrap() {
        ResponseFrame::Success(body) => OwnedResponse::Success(body.to_vec()),
        ResponseFrame::Failure(failure) => OwnedResponse::Failure(failure),
    }
}

enum OwnedResponse {
    Success(Vec<u8>),
    Failure(CallFailure),
}

#[tokio::test]
async fn iroh_claim_happy_path_and_deny_paths_match_weft_promotion() {
    let signer = Ed25519Signer::generate().unwrap();
    let secret = b"correct-link-secret".to_vec();
    let mut claim = state(hex::encode(signer.public_key()));
    assert!(claim.reissue(&secret, chrono::Utc::now().timestamp_millis() + 60_000));
    let authorization = Arc::new(MemoryClaimAuthorization {
        state: Mutex::new(claim),
        secret: secret.clone(),
        signer,
    });
    let (router, client, address) = endpoints(Arc::clone(&authorization)).await;

    let OwnedResponse::Failure(wrong_link) = call(
        &client,
        address.clone(),
        CLAIM_RESOLVE_METHOD,
        b"wrong-link-secret",
        br#"{"kind":"resolve"}"#,
    )
    .await
    else {
        panic!("wrong claim link must fail");
    };
    assert_eq!(wrong_link.code, CallFailureCode::Unauthenticated as i32);

    let OwnedResponse::Success(resolved) = call(
        &client,
        address.clone(),
        CLAIM_RESOLVE_METHOD,
        &secret,
        br#"{"kind":"resolve"}"#,
    )
    .await
    else {
        panic!("valid link must resolve");
    };
    let resolved: serde_json::Value = serde_json::from_slice(&resolved).unwrap();
    assert_eq!(resolved["agent"]["accountId"], OWNER_ID);

    let nonce = b"0123456789abcdef";
    let nonce_b64 = URL_SAFE_NO_PAD.encode(nonce);
    let pre_body = serde_json::json!({
        "kind": "preConsent",
        "handle": "human-handle",
        "nonce": nonce_b64,
    });
    let OwnedResponse::Success(pre) = call(
        &client,
        address.clone(),
        CLAIM_CONSENT_METHOD,
        &secret,
        &serde_json::to_vec(&pre_body).unwrap(),
    )
    .await
    else {
        panic!("pre-consent must succeed");
    };
    let pre: serde_json::Value = serde_json::from_slice(&pre).unwrap();
    let pre_consent = consent_from_reply(&pre);

    let credential_id = "Y3JlZGVudGlhbA";
    let promote_body = serde_json::json!({
        "kind": "promoteConsent",
        "handle": "human-handle",
        "credentialId": credential_id,
    });
    let OwnedResponse::Success(promote) = call(
        &client,
        address.clone(),
        CLAIM_CONSENT_METHOD,
        &secret,
        &serde_json::to_vec(&promote_body).unwrap(),
    )
    .await
    else {
        panic!("promote-consent must succeed");
    };
    let promote: serde_json::Value = serde_json::from_slice(&promote).unwrap();
    let promote_consent = consent_from_reply(&promote);

    let state = authorization.state.lock().await;
    let now = chrono::Utc::now().timestamp_millis();
    let mut account = MockWeftAccount {
        owner_id: state.owner_id,
        spool_owner_id: state.owner_id,
        root_credential_id: None,
    };
    let mut forged = account.clone();
    assert!(
        !forged.promote(
            authorization.signer.public_key(),
            &MockPromotion {
                handle: "human-handle",
                nonce,
                credential_id: "Zm9yZ2Vk",
                pre: &pre_consent,
                promote: &promote_consent,
                now_millis: now,
            },
        ),
        "a forged credential binding must be rejected"
    );
    assert!(account.promote(
        authorization.signer.public_key(),
        &MockPromotion {
            handle: "human-handle",
            nonce,
            credential_id,
            pre: &pre_consent,
            promote: &promote_consent,
            now_millis: now,
        },
    ));
    assert_eq!(
        account.root_credential_id.as_deref(),
        Some(credential_id),
        "accepted promotion attaches the passkey root"
    );
    assert_eq!(account.owner_id.to_string(), OWNER_ID);
    assert_eq!(
        account.spool_owner_id, account.owner_id,
        "spool ownership remains anchored to the stable owner UUID"
    );
    assert!(!account.promote(
        authorization.signer.public_key(),
        &MockPromotion {
            handle: "human-handle",
            nonce,
            credential_id,
            pre: &pre_consent,
            promote: &promote_consent,
            now_millis: now,
        },
    ));
    assert!(state.is_claimed());
    drop(state);

    let OwnedResponse::Failure(already_claimed) = call(
        &client,
        address,
        CLAIM_CONSENT_METHOD,
        &secret,
        &serde_json::to_vec(&promote_body).unwrap(),
    )
    .await
    else {
        panic!("already-claimed account must fail");
    };
    assert_eq!(
        already_claimed.code,
        CallFailureCode::Unauthenticated as i32,
        "a consumed link must stop authenticating before a second consent"
    );

    client.close().await;
    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_iroh_claim_link_is_rejected_before_dispatch() {
    let signer = Ed25519Signer::generate().unwrap();
    let secret = b"expired-link-secret".to_vec();
    let mut claim = state(hex::encode(signer.public_key()));
    assert!(claim.reissue(&secret, chrono::Utc::now().timestamp_millis() - 1));
    let authorization = Arc::new(MemoryClaimAuthorization {
        state: Mutex::new(claim),
        secret: secret.clone(),
        signer,
    });
    let (router, client, address) = endpoints(authorization).await;

    let OwnedResponse::Failure(expired) = call(
        &client,
        address,
        CLAIM_RESOLVE_METHOD,
        &secret,
        br#"{"kind":"resolve"}"#,
    )
    .await
    else {
        panic!("expired link must fail");
    };
    assert_eq!(expired.code, CallFailureCode::Unauthenticated as i32);

    client.close().await;
    router.shutdown().await.unwrap();
}
