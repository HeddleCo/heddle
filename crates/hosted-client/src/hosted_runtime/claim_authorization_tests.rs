//! Native device claim correctness: exact browser possession, root handoff and durable retries.
#![allow(clippy::result_large_err)]
use std::{
    ffi::OsString,
    net::Ipv4Addr,
    sync::{Arc, MutexGuard},
};

use api::{
    heddle::api::{
        v1alpha1::{CallContext, CallFailure, CallFailureCode},
        v2alpha1::*,
    },
    v2::client::Rpc as _,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use crypto::{Ed25519Signer, Signer as _};
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};
use prost::Message;
use thread_api::{credentials::Credentials, transport::Authorize as _};

use super::{
    claim_authorization::StoredClaimAuthorization,
    claim_native::handle_with_authority,
    hosted::claim_protocol::{
        CLAIM_PREPARE_METHOD, CLAIM_SIGN_METHOD, ClaimHandler, ClaimProtocol, ClaimSecretVerifier,
        NATIVE_ALPN, VerifiedClaimPrincipal,
    },
    identity_state::{self, ClaimState},
    owner_root::mint_and_record_claimable_root,
    root_mint::mint_agent_root,
};

struct IsolatedHome {
    _guard: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    previous: Option<OsString>,
}
impl IsolatedHome {
    fn new() -> Self {
        let guard = config::credentials::lock_test_env();
        let temp = tempfile::tempdir().expect("isolated claim home");
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", temp.path());
        }
        Self {
            _guard: guard,
            _temp: temp,
            previous,
        }
    }
}
impl Drop for IsolatedHome {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
        }
    }
}
struct Fixture {
    _home: IsolatedHome,
    signer: Ed25519Signer,
    token: String,
    browser: Arc<Ed25519Signer>,
    state: ClaimState,
}
impl Fixture {
    fn new() -> Self {
        let home = IsolatedHome::new();
        let signer = Ed25519Signer::from_seed(&[31; 32]).expect("owner signer");
        let root = mint_agent_root(&[31; 32]).expect("root authority");
        let mut state = ClaimState::new(
            "api.claim.test".into(),
            uuid::Uuid::new_v4(),
            root.subject,
            "quiet-otter".into(),
            hex::encode(signer.public_key()),
            None,
        );
        mint_and_record_claimable_root(&mut state, &signer, chrono::Utc::now().timestamp())
            .expect("root genesis");
        assert!(state.reissue(
            b"claim-secret",
            chrono::Utc::now().timestamp_millis() + 600_000
        ));
        identity_state::store(&state).expect("private claim state");
        Self {
            _home: home,
            signer,
            token: root.token,
            browser: Arc::new(Ed25519Signer::from_seed(&[32; 32]).expect("browser key")),
            state,
        }
    }
    fn principal(&self) -> VerifiedClaimPrincipal {
        VerifiedClaimPrincipal {
            subject: self.state.owner_id.to_string(),
            authorization_hash: self.state.authorization_hash().into(),
            browser_public_key: self.browser.public_key().to_vec(),
        }
    }
    fn prepare(&self) -> PrepareAccountClaimRequest {
        PrepareAccountClaimRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            browser_public_key: self.browser.public_key().to_vec(),
            handle: "human-handle".into(),
            claim_authorization: b"claim-secret".to_vec(),
        }
    }
    fn credentials(&self) -> Credentials {
        Credentials::Signed {
            signer: Arc::clone(&self.browser),
            biscuit: Vec::new(),
            grant_envelope: Vec::new(),
        }
    }
    fn handle(&self, method: &str, body: &[u8]) -> anyhow::Result<Vec<u8>> {
        handle_with_authority(
            &self.signer,
            self.token.as_bytes(),
            method,
            &self.principal(),
            body,
        )
    }
}
fn proposal(root: &SignedOwnerRoot, browser: &Ed25519Signer) -> SignedOwnerKeyTransition {
    let guardians = [
        Ed25519Signer::from_seed(&[33; 32]).expect("guardian one"),
        Ed25519Signer::from_seed(&[34; 32]).expect("guardian two"),
    ];
    let mut keys = guardians
        .iter()
        .map(|signer| RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(repo::ed25519_verification_key(signer.public_key()).expect("guardian key")),
        })
        .collect::<Vec<_>>();
    keys.sort_by_key(|guardian| repo::authorization_key_id(guardian.key.as_ref().expect("key")));
    let transition = repo::claim_deferred_human_transition(
        root,
        repo::ed25519_verification_key(browser.public_key()).expect("next key"),
        RecoveryPolicy {
            threshold: 2,
            guardians: keys,
            window_secs: None,
        },
        chrono::Utc::now().timestamp(),
        [9; 32],
    )
    .expect("claim body");
    let body = repo::owner_key_transition_body(&transition).expect("canonical body");
    let next_recovery_key_proofs = transition
        .next_recovery_policy
        .as_ref()
        .expect("policy")
        .guardians
        .iter()
        .map(|guardian| {
            let signer = guardians
                .iter()
                .find(|signer| {
                    signer.public_key() == guardian.key.as_ref().expect("key").public_key
                })
                .expect("guardian signer");
            repo::sign_canonical(signer, repo::OWNER_TRANSITION_DOMAIN, &body)
                .expect("guardian possession")
        })
        .collect();
    SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: Vec::new(),
        next_authority_key_proof: Some(
            repo::sign_canonical(browser, repo::OWNER_TRANSITION_DOMAIN, &body)
                .expect("browser possession"),
        ),
        next_recovery_key_proofs,
    }
}
fn sign_request(
    prepared: &PrepareAccountClaimResponse,
    browser: &Ed25519Signer,
) -> SignAccountClaimRequest {
    SignAccountClaimRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        claim_authorization: b"claim-secret".to_vec(),
        registration: Some(CompleteRegistrationRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            challenge: Some(RecordRef {
                id: "weft-owned-challenge".into(),
                ..Default::default()
            }),
            caller_public_key: browser.public_key().to_vec(),
            // Device checks presence/key binding; Weft verifies these passkey bytes.
            passkey: Some(PasskeyRegistration {
                credential_id: vec![1],
                client_data_json: vec![2],
                attestation_object: vec![3],
            }),
            device_binding: Some(PasskeyProof::default()),
            ..Default::default()
        }),
        proposed_transition: Some(proposal(
            prepared.owner_root.as_ref().expect("owner root"),
            browser,
        )),
    }
}

#[tokio::test]
async fn native_claim_requires_exact_browser_possession_and_active_secret() {
    let fixture = Fixture::new();
    let (authorization, _, _) = StoredClaimAuthorization::new();
    let request = fixture.prepare();
    let body = request.encode_to_vec();
    let context = fixture
        .credentials()
        .context(
            thread_api::rpc::OwnerAuthorizationServicePrepareAccountClaim::METHOD,
            &body,
        )
        .await
        .expect("signed context");
    let principal = authorization
        .verify(CLAIM_PREPARE_METHOD, &context, &body)
        .await
        .expect("admitted");
    assert_eq!(principal, fixture.principal());
    assert_eq!(
        authorization
            .verify(CLAIM_PREPARE_METHOD, &CallContext::default(), &body)
            .await
            .expect_err("proof required")
            .code,
        CallFailureCode::Unauthenticated as i32
    );
    let mut changed = request.clone();
    changed.handle = "different".into();
    assert!(
        authorization
            .verify(CLAIM_PREPARE_METHOD, &context, &changed.encode_to_vec())
            .await
            .is_err(),
        "exact bytes bound"
    );
    changed = request;
    changed.claim_authorization = b"wrong-secret".to_vec();
    let changed_body = changed.encode_to_vec();
    let changed_context = fixture
        .credentials()
        .context(
            thread_api::rpc::OwnerAuthorizationServicePrepareAccountClaim::METHOD,
            &changed_body,
        )
        .await
        .expect("new proof");
    assert_eq!(
        authorization
            .verify(CLAIM_PREPARE_METHOD, &changed_context, &changed_body)
            .await
            .expect_err("secret required")
            .code,
        CallFailureCode::Unauthenticated as i32
    );
}

#[tokio::test]
async fn native_claim_preserves_root_and_replays_exact_receipts_after_restart() {
    let fixture = Fixture::new();
    let prepare_body = fixture.prepare().encode_to_vec();
    let prepare_bytes = fixture
        .handle(CLAIM_PREPARE_METHOD, &prepare_body)
        .expect("prepare");
    let prepared =
        PrepareAccountClaimResponse::decode(prepare_bytes.as_slice()).expect("native reply");
    assert_eq!(prepared.display_name, "quiet-otter");
    let root_key = super::root_mint::authority_keypair(&[31; 32])
        .expect("authority")
        .public();
    for operation in ["BeginRegistration", "CompleteRegistration"] {
        let verified = biscuit_verifier::verify_any_at_with_resource(
            &URL_SAFE.encode(&prepared.ceremony_biscuit),
            None,
            &[root_key],
            &[],
            operation,
            None,
            chrono::Utc::now(),
        )
        .expect("ceremony allowed");
        assert_eq!(
            verified.cnf,
            Some(hex::encode(fixture.browser.public_key()))
        );
    }
    assert!(
        biscuit_verifier::verify_any_at_with_resource(
            &URL_SAFE.encode(&prepared.ceremony_biscuit),
            None,
            &[root_key],
            &[],
            "CreateSpool",
            None,
            chrono::Utc::now()
        )
        .is_err()
    );
    let sign = sign_request(&prepared, fixture.browser.as_ref());
    let mut mismatched = sign.clone();
    mismatched
        .proposed_transition
        .as_mut()
        .expect("proposal")
        .transition
        .as_mut()
        .expect("body")
        .sequence += 1;
    assert!(
        fixture
            .handle(CLAIM_SIGN_METHOD, &mismatched.encode_to_vec())
            .is_err(),
        "exact root transition required"
    );
    let sign_body = sign.encode_to_vec();
    let signed_bytes = fixture
        .handle(CLAIM_SIGN_METHOD, &sign_body)
        .expect("cosign");
    let signed = SignAccountClaimResponse::decode(signed_bytes.as_slice())
        .expect("native response")
        .transition
        .expect("transition");
    let proposed = sign.proposed_transition.expect("proposal");
    assert_eq!(signed.transition, proposed.transition);
    assert_eq!(
        signed.next_authority_key_proof,
        proposed.next_authority_key_proof
    );
    assert_eq!(
        signed.next_recovery_key_proofs,
        proposed.next_recovery_key_proofs
    );
    assert_eq!(signed.authorizations.len(), 1);
    let persisted = identity_state::load()
        .expect("reload state")
        .expect("state");
    assert!(persisted.consent_issued());
    let (restarted, _, _) = StoredClaimAuthorization::new();
    assert_eq!(
        restarted
            .call(CLAIM_PREPARE_METHOD, fixture.principal(), &prepare_body)
            .await
            .expect("durable preparation receipt"),
        prepare_bytes
    );
    assert_eq!(
        restarted
            .call(CLAIM_SIGN_METHOD, fixture.principal(), &sign_body)
            .await
            .expect("durable signature receipt"),
        signed_bytes
    );
    assert!(
        restarted
            .call(
                CLAIM_SIGN_METHOD,
                fixture.principal(),
                &mismatched.encode_to_vec()
            )
            .await
            .is_err(),
        "same operation cannot mutate payload"
    );
}

struct Foreground {
    signer: Ed25519Signer,
    token: String,
}
impl std::fmt::Debug for Foreground {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForegroundClaimSigner")
    }
}
impl ClaimHandler for Foreground {
    async fn call(
        &self,
        method: &str,
        principal: VerifiedClaimPrincipal,
        body: &[u8],
    ) -> Result<Vec<u8>, CallFailure> {
        handle_with_authority(
            &self.signer,
            self.token.as_bytes(),
            method,
            &principal,
            body,
        )
        .map_err(|error| CallFailure {
            code: CallFailureCode::FailedPrecondition as i32,
            message: error.to_string(),
            error: None,
        })
    }
}
#[tokio::test]
async fn native_claim_crosses_real_iroh_with_typed_endpoint_discovery() {
    let fixture = Fixture::new();
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("listen address")
        .bind()
        .await
        .expect("device endpoint");
    let address = endpoint.addr();
    let key = *endpoint.id().as_bytes();
    // Match the persisted physical endpoint used in command receipts.
    let mut state = identity_state::load().expect("load").expect("state");
    state.node_id = hex::encode(key);
    identity_state::store(&state).expect("endpoint state");
    let (authorization, _, _) = StoredClaimAuthorization::new();
    let router = Router::builder(endpoint)
        .accept(
            NATIVE_ALPN,
            ClaimProtocol::new(
                Arc::new(authorization),
                Arc::new(Foreground {
                    signer: Ed25519Signer::from_seed(&[31; 32]).expect("owner signer"),
                    token: fixture.token.clone(),
                }),
                key,
            ),
        )
        .spawn();
    let browser_endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("browser address")
        .bind()
        .await
        .expect("browser endpoint");
    let connection = browser_endpoint
        .connect(address, NATIVE_ALPN)
        .await
        .expect("native connection");
    let transport = thread_api::transport::IrohTransport::new(
        connection,
        fixture.credentials(),
        256 * 1024,
        std::time::Duration::from_secs(5),
    )
    .expect("native transport");
    let remote = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("native device discovery");
    let prepared = remote
        .api
        .call::<thread_api::rpc::OwnerAuthorizationServicePrepareAccountClaim>(&fixture.prepare())
        .await
        .expect("typed prepare");
    assert_eq!(
        prepared
            .owner_root
            .as_ref()
            .expect("root")
            .root
            .as_ref()
            .expect("body")
            .account_uuid,
        fixture.state.owner_id.as_bytes()
    );
    let result = remote
        .api
        .call::<thread_api::rpc::OwnerAuthorizationServiceSignAccountClaim>(&sign_request(
            &prepared,
            fixture.browser.as_ref(),
        ))
        .await
        .expect("typed cosign");
    assert_eq!(
        result.transition.expect("transition").authorizations.len(),
        1
    );
    browser_endpoint.close().await;
    router.shutdown().await.expect("device shutdown");
}
