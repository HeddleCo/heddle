use std::{ffi::OsString, sync::MutexGuard};

use chrono::{Duration, Utc};
use config::credentials::{self, ServerCredential};
use crypto::{Ed25519Signer, Signer as _};
use objects::HeddleError;
use tempfile::TempDir;

use super::{
    agent_node_identity,
    auth::headless_token_metadata,
    auth_login::{LoginInputs, LoginPath, login, login_path, store_agent_root},
    auth_login_agent::{
        finish_invite_create_from_response, provision_response_for_test,
        remint_with_client_for_test, test_support::start_recording_client,
    },
    auth_requests::{AuthOptions, LoginPermission},
    device_flow::restrict_agent_account_root,
    identity_state::{self, ClaimState},
    root_mint::mint_agent_root,
};

async fn run_login(
    server: &str,
    permission: LoginPermission,
    invite: Option<String>,
) -> anyhow::Result<super::auth::AuthLoginOutcome> {
    login(
        &AuthOptions::default(),
        server,
        permission,
        invite,
        &mut |_| Ok(()),
    )
    .await
}

struct IsolatedHome {
    _guard: MutexGuard<'static, ()>,
    _temp: TempDir,
    prev_home: Option<OsString>,
    prev_heddle_home: Option<OsString>,
    prev_credential: Option<OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let guard = credentials::lock_test_env();
        let temp = TempDir::new().expect("temp home");
        let prev_home = std::env::var_os("HOME");
        let prev_heddle_home = std::env::var_os("HEDDLE_HOME");
        let prev_credential = std::env::var_os("HEDDLE_CREDENTIAL");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("HEDDLE_HOME");
            std::env::remove_var("HEDDLE_CREDENTIAL");
        }
        Self {
            _guard: guard,
            _temp: temp,
            prev_home,
            prev_heddle_home,
            prev_credential,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_heddle_home {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
            match &self.prev_credential {
                Some(value) => std::env::set_var("HEDDLE_CREDENTIAL", value),
                None => std::env::remove_var("HEDDLE_CREDENTIAL"),
            }
        }
    }
}

#[test]
fn login_path_covers_the_four_locked_routes() {
    let reuse = LoginInputs {
        reusable_cred: true,
        node_key_account: true,
        has_invite: true,
        browser_allowed: true,
    };
    assert_eq!(login_path(reuse), LoginPath::Reuse);

    let remint = LoginInputs {
        reusable_cred: false,
        node_key_account: true,
        has_invite: true,
        browser_allowed: false,
    };
    assert_eq!(login_path(remint), LoginPath::Remint);

    let invite = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: true,
        browser_allowed: false,
    };
    assert_eq!(login_path(invite), LoginPath::CreateWithInvite);

    let browser = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: false,
        browser_allowed: true,
    };
    assert_eq!(login_path(browser), LoginPath::Browser);

    let fail_closed = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: false,
        browser_allowed: false,
    };
    assert_eq!(login_path(fail_closed), LoginPath::FailClosed);
}

fn store_device_cred(server: &str, expires_at: Option<chrono::DateTime<Utc>>) -> String {
    let signer = Ed25519Signer::generate().expect("device key");
    let mut builder = biscuit_auth::Biscuit::builder()
        .fact(r#"user("alice")"#)
        .expect("user fact")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
        .expect("device PoP fact");
    if let Some(expires_at) = expires_at {
        builder = builder
            .fact(format!("expires_at({})", expires_at.to_rfc3339()).as_str())
            .expect("expiry fact");
    }
    let token = builder
        .build(&biscuit_auth::KeyPair::new())
        .expect("build token")
        .to_base64()
        .expect("encode token");
    credentials::store_server_credential(
        server,
        ServerCredential {
            token: token.clone(),
            subject: "alice".to_string(),
            device_id: None,
            credential_id: None,
            private_key_pem: Some(signer.to_pem().expect("pem")),
            expires_at: expires_at.map(|value| value.to_rfc3339()),
        },
    )
    .expect("store credential");
    token
}

#[tokio::test]
async fn login_reuses_a_valid_unexpired_credential_without_minting() {
    let _home = IsolatedHome::new();
    let server = "api.reuse.test";
    let token = store_device_cred(server, Some(Utc::now() + Duration::hours(2)));
    run_login(server, LoginPermission::HeadlessOnly, None)
        .await
        .expect("reuse must succeed");
    let stored = credentials::get_server_credential(server)
        .expect("load")
        .expect("still stored");
    assert_eq!(stored.token, token, "reuse must not remint");
}

#[tokio::test]
async fn login_reuses_a_credential_that_has_no_stored_expiry() {
    let _home = IsolatedHome::new();
    let server = "api.reuse-no-expiry.test";
    let token = store_device_cred(server, None);
    run_login(server, LoginPermission::HeadlessOnly, None)
        .await
        .expect("missing expiry is still a valid stored cred");
    let stored = credentials::get_server_credential(server)
        .expect("load")
        .expect("still stored");
    assert_eq!(stored.token, token, "reuse must not remint");
}

#[tokio::test]
async fn login_remints_an_expired_node_key_account_without_an_invite() {
    let _home = IsolatedHome::new();
    let server = "api.remint.test";
    let identity = agent_node_identity::load_or_create().expect("node identity");
    let seed = identity.secret_key().to_bytes();
    let signer = Ed25519Signer::from_seed(&seed).expect("signer");
    let root = mint_agent_root(&seed).expect("mint");
    let restricted =
        restrict_agent_account_root(&root.token, &signer, root.expires_at).expect("restrict");
    let expired = Utc::now() - Duration::hours(1);
    store_agent_root(
        server,
        restricted.clone(),
        root.subject.clone(),
        root.private_key_pem.clone(),
        expired,
    )
    .expect("store expired");
    run_login(server, LoginPermission::HeadlessOnly, None)
        .await
        .expect("remint must succeed without invite");
    let stored = credentials::get_server_credential(server)
        .expect("load")
        .expect("reminted");
    assert_ne!(
        stored.token, restricted,
        "remint must replace the expired token"
    );
    let expires = stored.expires_at.expect("refreshed expiry");
    let parsed = chrono::DateTime::parse_from_rfc3339(&expires)
        .expect("rfc3339")
        .with_timezone(&Utc);
    assert!(parsed > Utc::now(), "reminted expiry must be in the future");
    let metadata = headless_token_metadata(&stored.token).expect("metadata");
    assert!(
        metadata
            .proof_public_key_hex
            .eq_ignore_ascii_case(&identity.node_id().to_string())
    );
}

#[tokio::test]
async fn login_fail_closed_without_browser_permission_invite_or_account() {
    let _home = IsolatedHome::new();
    let error = run_login("api.heddle.sh", LoginPermission::HeadlessOnly, None)
        .await
        .expect_err("non-TTY login must fail closed");
    let domain = error.downcast_ref::<HeddleError>().expect("typed refusal");
    let HeddleError::Recovery(advice) = domain else {
        panic!("expected recovery refusal")
    };
    assert_eq!(advice.kind, "auth_login_invite_required");
    assert_eq!(
        advice.recovery_commands.as_deref(),
        Some(["heddle auth login --invite <code>".to_string()].as_slice())
    );
    assert!(
        !agent_node_identity::identity_path().exists(),
        "fail-closed must not mint a node key"
    );
}

#[tokio::test]
async fn login_with_invite_does_not_take_the_fail_closed_path() {
    let _home = IsolatedHome::new();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_login(
            "https://127.0.0.1:1",
            LoginPermission::HeadlessOnly,
            Some("invite-secret".to_string()),
        ),
    )
    .await
    .expect("invite login must not hang on a claim URL")
    .expect_err("invite create still needs a reachable server");
    let message = error.to_string();
    assert!(
        error.downcast_ref::<HeddleError>().is_none_or(|domain| {
            !matches!(domain, HeddleError::Recovery(advice) if advice.kind == "auth_login_invite_required")
        }),
        "invite must not fail closed: {message}"
    );
}

#[test]
fn login_invite_create_succeeds_with_a_claim_next_directive() {
    let _home = IsolatedHome::new();
    let server = "api.claim-next.test";
    let output = finish_invite_create_from_response(
        server,
        provision_response_for_test(
            "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28",
            "quiet-otter",
            "https://claims.heddle.test/",
        ),
    )
    .expect("invite create must succeed without a server claim token");
    assert_eq!(output.account_id, "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28");
    assert!(!output.subject.is_empty());
    assert!(
        credentials::get_server_credential(server)
            .expect("load credential")
            .is_some(),
        "successful create must persist its agent credential"
    );
    let state = identity_state::load()
        .expect("load claim state")
        .expect("claim state was stored");
    assert_eq!(state.server, server);
    assert_eq!(state.pet_name, "quiet-otter");
    assert_eq!(
        state.web_origin.as_deref(),
        Some("https://claims.heddle.test/")
    );
    assert!(
        state.signed_owner_root_hex.is_some(),
        "invite create must mint the claimable deferred-human owner root"
    );
}

#[tokio::test]
async fn remint_uses_claim_state_and_uploads_owner_root_at_enrollment() {
    let _home = IsolatedHome::new();
    let server = "api.claim-state.test";
    let identity = agent_node_identity::load_or_create().expect("node identity");
    identity_state::store(&ClaimState::new(
        server.to_string(),
        uuid::Uuid::parse_str("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28").expect("uuid"),
        "subject-1".to_string(),
        "quiet-otter".to_string(),
        identity.node_id().to_string(),
        None,
    ))
    .expect("store claim state");
    let (mut client, server_task, calls, _) = start_recording_client().await;
    remint_with_client_for_test(server, &mut client)
        .await
        .expect("missing cred + claim state remints and uploads");
    client.close().await;
    server_task.await.expect("recording server");
    let stored = credentials::get_server_credential(server)
        .expect("load")
        .expect("reminted into the keystore");
    assert!(!stored.token.is_empty());
    let state = identity_state::load().expect("load").expect("claim state");
    assert!(
        state.signed_owner_root_hex.is_some(),
        "remint must lazy-mint the claimable deferred-human owner root"
    );
    assert_eq!(
        *calls.lock().unwrap_or_else(|poison| poison.into_inner()),
        [
            "/heddle.api.v2alpha1.EndpointService/DescribeEndpoint",
            "/heddle.api.v2alpha1.OwnerAuthorizationService/BootstrapOwnership"
        ],
        "remint must install the owner root during enrollment"
    );
}

#[test]
fn provisioned_agent_retains_registered_session_and_rejects_changed_key() {
    use api::heddle::api::v2alpha1 as v2;
    let _home = IsolatedHome::new();
    let server = "api.native-agent.test";
    let response = provision_response_for_test(
        "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28",
        "quiet-otter",
        "https://claims.heddle.test/",
    );
    for altered in 0..4 {
        let mut changed = response.clone();
        let result = changed.credential.as_mut().expect("credential");
        let Some(v2::credential_result::Outcome::ClientOwned(credential)) = result.outcome.as_mut()
        else {
            panic!("client-owned fixture")
        };
        match altered {
            0 => credential.proof_public_key[0] ^= 1,
            1 => credential.subject = "another-agent".into(),
            2 => credential.kind = v2::CredentialKind::Device as i32,
            _ => result.session.as_mut().expect("session").revoked = true,
        }
        finish_invite_create_from_response(server, changed)
            .expect_err("altered agent registration must not be stored");
        assert!(
            credentials::get_server_credential(server)
                .expect("credential store")
                .is_none()
        );
        assert!(identity_state::load().expect("claim state").is_none());
    }
    finish_invite_create_from_response(server, response).expect("valid registration");
    let stored = credentials::get_server_credential(server)
        .expect("credential")
        .expect("saved");
    assert_eq!(
        stored.credential_id.as_deref(),
        Some("fixture-agent-credential")
    );
    assert_eq!(
        super::root_mint::authority_session_fact(&stored.token).expect("registered session"),
        "fixture-agent-session"
    );
    let biscuit =
        biscuit_auth::UnverifiedBiscuit::from_base64(&stored.token).expect("registered Biscuit");
    assert!(
        biscuit
            .print_block_source(0)
            .expect("authority block")
            .contains("fixture-agent-credential")
    );
}

#[test]
fn provisioning_reuse_recovers_the_original_owner_root_after_local_state_loss() {
    use api::heddle::api::v2alpha1 as v2;
    let _home = IsolatedHome::new();
    let server = "api.native-reuse.test";
    let mut response = provision_response_for_test(
        "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28",
        "quiet-otter",
        "https://claims.heddle.test/",
    );
    finish_invite_create_from_response(server, response.clone())
        .expect("first account registration");
    let original = identity_state::load().expect("state").expect("claim state");
    let root = super::owner_root::load_recorded_root(&original)
        .expect("root")
        .expect("original root");
    let credential = credentials::get_server_credential(server)
        .expect("credential")
        .expect("saved");
    let signer = Ed25519Signer::from_pem(credential.private_key_pem.as_deref().expect("key"))
        .expect("signer");
    let binding = repo::sign_agent_claim_binding(&signer, &root, "fixture-bootstrap")
        .expect("original binding");
    let verified = heddleco_capability_verifier::verify_owner_root(&root).expect("verified root");
    response.ownership = Some(v2::OwnerState {
        owner: Some(v2::PrincipalRef {
            id: original.owner_id.to_string(),
        }),
        root: Some(root.clone()),
        binding: Some(binding),
        version: verified.state_hash().to_vec(),
        ..Default::default()
    });
    std::fs::remove_file(identity_state::state_path())
        .expect("simulate only local claim-state loss");
    finish_invite_create_from_response(server, response.clone())
        .expect("reuse recovers hosted original proof");
    let recovered = identity_state::load().expect("state").expect("recovered");
    assert_eq!(
        super::owner_root::load_recorded_root(&recovered).expect("root"),
        Some(root)
    );
    let persisted = std::fs::read(identity_state::state_path()).expect("persisted state");
    response.ownership.as_mut().expect("ownership").version[0] ^= 1;
    finish_invite_create_from_response(server, response)
        .expect_err("incorrect authority hash rejected");
    assert_eq!(
        std::fs::read(identity_state::state_path()).expect("state"),
        persisted,
        "failed proof cannot replace the recovered original"
    );
}
