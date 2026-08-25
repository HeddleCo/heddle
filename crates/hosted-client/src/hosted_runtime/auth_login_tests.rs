use std::{ffi::OsString, sync::MutexGuard};

use api::heddle::api::v1alpha1::CreateAgentAccountResponse;
use chrono::{Duration, Utc};
use config::credentials::{self, ServerCredential};
use crypto::{Ed25519Signer, Signer as _};
use heddle_cli_args::CliContext;
use tempfile::TempDir;

use super::{
    agent_node_identity,
    auth::headless_token_metadata,
    auth_login::{LoginInputs, LoginPath, login, login_path, store_agent_root},
    auth_login_agent::{finish_invite_create_from_response, invite_created_claim_link},
    device_flow::restrict_agent_account_root,
    identity_state::{self, ClaimState},
    root_mint::mint_agent_root,
};

struct TextCtx;

impl CliContext for TextCtx {
    fn repo_path(&self) -> Option<&std::path::Path> {
        None
    }
    fn operation_id_wire(&self) -> String {
        String::new()
    }
    fn should_output_json(&self, _repo_config: Option<&repo::Config>) -> bool {
        false
    }
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
fn login_path_covers_the_five_locked_routes() {
    let reuse = LoginInputs {
        reusable_cred: true,
        node_key_account: true,
        has_invite: true,
        interactive: true,
        force_browser: true,
    };
    assert_eq!(login_path(reuse), LoginPath::Reuse);

    let remint = LoginInputs {
        reusable_cred: false,
        node_key_account: true,
        has_invite: true,
        interactive: false,
        force_browser: false,
    };
    assert_eq!(login_path(remint), LoginPath::Remint);

    let invite = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: true,
        interactive: false,
        force_browser: false,
    };
    assert_eq!(login_path(invite), LoginPath::CreateWithInvite);

    let browser = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: false,
        interactive: true,
        force_browser: false,
    };
    assert_eq!(login_path(browser), LoginPath::Browser);

    let forced = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: false,
        interactive: false,
        force_browser: true,
    };
    assert_eq!(login_path(forced), LoginPath::Browser);

    let fail_closed = LoginInputs {
        reusable_cred: false,
        node_key_account: false,
        has_invite: false,
        interactive: false,
        force_browser: false,
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
    login(&TextCtx, server, false, None, false)
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
    login(&TextCtx, server, false, None, false)
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
    login(&TextCtx, server, false, None, false)
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
async fn login_fail_closed_without_tty_invite_or_account() {
    let _home = IsolatedHome::new();
    let error = login(&TextCtx, "api.heddle.sh", false, None, false)
        .await
        .expect_err("non-TTY login must fail closed");
    let advice = error
        .downcast_ref::<heddle_cli_contract::cli::commands::RecoveryAdvice>()
        .expect("typed refusal");
    assert_eq!(advice.kind, "auth_login_invite_required");
    assert_eq!(advice.primary_command, "heddle auth login --invite <code>");
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
        login(
            &TextCtx,
            "https://127.0.0.1:1",
            false,
            Some("invite-secret".to_string()),
            false,
        ),
    )
    .await
    .expect("invite login must not hang on a claim URL")
    .expect_err("invite create still needs a reachable server");
    let message = error.to_string();
    assert!(
        error
            .downcast_ref::<heddle_cli_contract::cli::commands::RecoveryAdvice>()
            .is_none_or(|advice| advice.kind != "auth_login_invite_required"),
        "invite must not fail closed: {message}"
    );
}

#[test]
fn login_invite_create_exposes_the_server_claim_token() {
    let _home = IsolatedHome::new();
    let token = "hcl1.node.one-time-claim";
    let claim_link = finish_invite_create_from_response(
        "api.claim-token.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            claim_token: token.into(),
        },
    )
    .expect("invite create must expose the server claim token");
    assert_eq!(claim_link, token);
}

#[test]
fn login_invite_create_refuses_to_drop_an_empty_claim_token() {
    let error = invite_created_claim_link("").expect_err("empty claim token must not be dropped");
    assert!(
        error.to_string().contains("claim token"),
        "missing token must stay visible: {error}"
    );
    let error = invite_created_claim_link("   ").expect_err("whitespace is not a claim token");
    assert!(
        error.to_string().contains("claim token"),
        "whitespace token must stay visible: {error}"
    );
}

#[tokio::test]
async fn remint_uses_claim_state_when_the_keystore_row_is_missing() {
    let _home = IsolatedHome::new();
    let server = "api.claim-state.test";
    let identity = agent_node_identity::load_or_create().expect("node identity");
    identity_state::store(&ClaimState::new(
        server.to_string(),
        uuid::Uuid::parse_str("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28").expect("uuid"),
        "subject-1".to_string(),
        "quiet-otter".to_string(),
        identity.node_id().to_string(),
    ))
    .expect("store claim state");
    login(&TextCtx, server, false, None, false)
        .await
        .expect("missing cred + claim state remints");
    let stored = credentials::get_server_credential(server)
        .expect("load")
        .expect("reminted into the keystore");
    assert!(!stored.token.is_empty());
}
