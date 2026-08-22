//! Local PoP-binding checks for [`super::HostedSession::build`].
//!
//! A PoP-bound bearer must fail in session construction when no matching
//! leaf key is available. These tests never open a connection.

use std::path::{Path, PathBuf};

use config::{UserConfig, credentials};
use crypto::{Ed25519Signer, Signer};

use super::{CallContextFactory, HostedAuthMode, HostedSession, context::SignedCallContext};

const SERVER: &str = "api.heddle.test";

fn mint_pop_token(subject: &str, signer: &Ed25519Signer) -> String {
    biscuit_auth::Biscuit::builder()
        .fact(format!("user(\"{subject}\")").as_str())
        .expect("user fact")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
        .expect("proof key fact")
        .build(&biscuit_auth::KeyPair::new())
        .expect("mint token")
        .to_base64()
        .expect("encode token")
}

fn mint_unbound_token(subject: &str) -> String {
    biscuit_auth::Biscuit::builder()
        .fact(format!("user(\"{subject}\")").as_str())
        .expect("user fact")
        .build(&biscuit_auth::KeyPair::new())
        .expect("mint unbound token")
        .to_base64()
        .expect("encode unbound token")
}

fn mint_derived_child(parent_token: &str, parent: &Ed25519Signer, child: &Ed25519Signer) -> String {
    crate::hosted_runtime::device_flow::attenuate_for_agent(
        parent_token,
        crate::hosted_runtime::device_flow::AgentAttenuation {
            agent_id: "agent-review".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            allowed_operations: Some(vec!["WhoAmI".to_string()]),
            allowed_resources: None,
            declared_scopes: Vec::new(),
        },
        parent,
        child.public_key(),
    )
    .expect("derive child token")
}

fn store_credential(token: &str, subject: &str, proof_key_pem: Option<String>) {
    credentials::store_server_credential(
        SERVER,
        credentials::ServerCredential {
            token: token.to_string(),
            subject: subject.to_string(),
            device_id: None,
            credential_id: None,
            private_key_pem: proof_key_pem,
            expires_at: None,
        },
    )
    .expect("store credential");
}

fn write_hcred(path: &Path, subject: &str, token: &str, proof_key_pem: &str) {
    crate::hosted_runtime::credential_file::write_credential_file(
        path,
        &crate::hosted_runtime::credential_file::VerifiedCredential {
            server: SERVER.to_string(),
            kind: crate::hosted_runtime::credential_file::CredentialKind::Device,
            subject: subject.to_string(),
            token: token.to_string(),
            proof_key_pem: proof_key_pem.to_string(),
            expires_at: None,
            credential_id: None,
            provenance: None,
        },
    )
    .expect("write .hcred");
}

fn user_config_with_proof_key(path: PathBuf) -> UserConfig {
    let mut user_config = UserConfig::default();
    user_config.remote.auth_proof_key_pem_path = Some(path);
    user_config
}

fn build_session(user_config: &UserConfig) -> anyhow::Result<HostedSession> {
    HostedSession::build(
        user_config,
        Some(SERVER.to_string()),
        HostedAuthMode::CredentialFallback,
    )
}

fn whoami_context(session: &HostedSession) -> SignedCallContext {
    CallContextFactory::from_client_config(session.client_config())
        .expect("session config must be a valid call context")
        .unary("/heddle.api.v1alpha1.IdentityService/WhoAmI", &[], "")
        .expect("build WhoAmI context")
}

fn assert_proof_attached(session: &HostedSession, expected_pem: &str) {
    assert_eq!(
        session.client_config().auth_proof_key_pem.as_deref(),
        Some(expected_pem)
    );
    assert!(
        whoami_context(session).context.bearer_proof.is_some(),
        "a matching leaf key must attach proof before any RPC"
    );
}

fn assert_token_only(session: &HostedSession) {
    assert!(session.client_config().auth_proof_key_pem.is_none());
    assert!(
        whoami_context(session).context.bearer_proof.is_none(),
        "an unbound bearer must keep its documented token-only behavior"
    );
}

fn with_isolated_env<T>(run: impl FnOnce(&Path) -> T) -> T {
    let _guard = credentials::lock_test_env();
    let home = tempfile::TempDir::new().expect("temp Heddle home");
    let previous_home = std::env::var_os("HEDDLE_HOME");
    let previous_credential = std::env::var_os("HEDDLE_CREDENTIAL");
    unsafe {
        std::env::set_var("HEDDLE_HOME", home.path());
        std::env::remove_var("HEDDLE_CREDENTIAL");
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(home.path())));
    unsafe {
        match previous_home {
            Some(path) => std::env::set_var("HEDDLE_HOME", path),
            None => std::env::remove_var("HEDDLE_HOME"),
        }
        match previous_credential {
            Some(path) => std::env::set_var("HEDDLE_CREDENTIAL", path),
            None => std::env::remove_var("HEDDLE_CREDENTIAL"),
        }
    }
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[test]
fn environment_credential_attaches_matching_proof() {
    with_isolated_env(|home| {
        let signer = Ed25519Signer::generate().expect("env proof key");
        let pem = signer.to_pem().expect("env PEM");
        let token = mint_pop_token("alice", &signer);
        let path = home.join("agent.hcred");
        write_hcred(&path, "alice", &token, &pem);
        unsafe { std::env::set_var("HEDDLE_CREDENTIAL", &path) };

        let session = build_session(&UserConfig::default()).expect("HEDDLE_CREDENTIAL session");
        assert_eq!(
            session
                .client_config()
                .token
                .as_ref()
                .map(|token| token.id.as_str()),
            Some(token.as_str())
        );
        assert_proof_attached(&session, &pem);
    });
}

#[test]
fn credential_store_root_attaches_matching_proof() {
    with_isolated_env(|_| {
        let signer = Ed25519Signer::generate().expect("root proof key");
        let pem = signer.to_pem().expect("root PEM");
        store_credential(
            &mint_pop_token("alice", &signer),
            "alice",
            Some(pem.clone()),
        );

        let session = build_session(&UserConfig::default()).expect("keystore root session");
        assert_proof_attached(&session, &pem);
    });
}

#[test]
fn credential_store_derived_child_attaches_matching_leaf_key() {
    with_isolated_env(|_| {
        let parent = Ed25519Signer::generate().expect("parent proof key");
        let child = Ed25519Signer::generate().expect("child proof key");
        let parent_token = mint_pop_token("alice", &parent);
        let child_token = mint_derived_child(&parent_token, &parent, &child);
        let pem = child.to_pem().expect("child PEM");
        store_credential(&child_token, "alice", Some(pem.clone()));

        let session = build_session(&UserConfig::default()).expect("keystore child session");
        assert_proof_attached(&session, &pem);
    });
}

#[test]
fn pop_bound_service_account_attaches_matching_proof() {
    with_isolated_env(|_| {
        let signer = Ed25519Signer::generate().expect("service-account key");
        let pem = signer.to_pem().expect("service-account PEM");
        store_credential(
            &mint_pop_token("sa:github-ci", &signer),
            "sa:github-ci",
            Some(pem.clone()),
        );

        let session = build_session(&UserConfig::default()).expect("service-account session");
        assert_proof_attached(&session, &pem);
    });
}

#[test]
fn configured_key_attaches_matching_proof() {
    with_isolated_env(|home| {
        let signer = Ed25519Signer::generate().expect("configured proof key");
        let pem = signer.to_pem().expect("configured PEM");
        let pem_path = home.join("proof.pem");
        std::fs::write(&pem_path, &pem).expect("write configured PEM");
        store_credential(&mint_pop_token("alice", &signer), "alice", None);

        let session =
            build_session(&user_config_with_proof_key(pem_path)).expect("configured-key session");
        assert_proof_attached(&session, &pem);
    });
}

#[test]
fn same_host_identity_attaches_matching_root_key() {
    with_isolated_env(|_| {
        let signer = Ed25519Signer::generate().expect("device key");
        let pem = signer.to_pem().expect("device PEM");
        let token = mint_pop_token("alice", &signer);
        store_credential(&token, "alice", None);
        repo::identity::link_device_key(signer.public_key(), &pem, SERVER)
            .expect("link same-host identity");

        let session = build_session(&UserConfig::default()).expect("same-host session");
        assert_proof_attached(&session, &pem);
    });
}

#[test]
fn pop_bound_credential_store_token_without_key_fails_before_connect() {
    with_isolated_env(|_| {
        store_credential(
            &mint_pop_token("alice", &Ed25519Signer::generate().expect("unused key")),
            "alice",
            None,
        );

        let Err(error) = build_session(&UserConfig::default()) else {
            panic!("a PoP-bound keystore token with no key must fail locally");
        };
        let message = error.to_string();
        assert!(
            message.contains("proof-of-possession") && message.contains("no matching"),
            "missing-key error must be actionable: {message}"
        );
    });
}

#[test]
fn same_host_identity_does_not_satisfy_a_derived_child() {
    with_isolated_env(|_| {
        let parent = Ed25519Signer::generate().expect("parent device key");
        let child = Ed25519Signer::generate().expect("child leaf key");
        let parent_pem = parent.to_pem().expect("parent PEM");
        let parent_token = mint_pop_token("alice", &parent);
        let child_token = mint_derived_child(&parent_token, &parent, &child);
        store_credential(&child_token, "alice", None);
        repo::identity::link_device_key(parent.public_key(), &parent_pem, SERVER)
            .expect("link ancestor identity");

        let Err(error) = build_session(&UserConfig::default()) else {
            panic!("a token-only derived child must not borrow the ancestor same-host key");
        };
        let message = error.to_string();
        assert!(
            message.contains("proof-of-possession") && message.contains("no matching"),
            "child-without-leaf-key error must be actionable: {message}"
        );
    });
}

#[test]
fn credential_store_mismatched_key_fails_locally() {
    with_isolated_env(|_| {
        let bound = Ed25519Signer::generate().expect("bound key");
        let other = Ed25519Signer::generate().expect("other key");
        store_credential(
            &mint_pop_token("alice", &bound),
            "alice",
            Some(other.to_pem().expect("other PEM")),
        );

        let Err(error) = build_session(&UserConfig::default()) else {
            panic!("a keystore key that is not the leaf key must fail locally");
        };
        assert!(
            error.to_string().contains("does not match"),
            "mismatch error must be actionable: {error}"
        );
    });
}

#[test]
fn configured_key_mismatch_fails_locally() {
    with_isolated_env(|home| {
        let bound = Ed25519Signer::generate().expect("bound key");
        let other = Ed25519Signer::generate().expect("other key");
        let pem_path = home.join("wrong.pem");
        std::fs::write(&pem_path, other.to_pem().expect("wrong PEM")).expect("write wrong PEM");
        store_credential(&mint_pop_token("alice", &bound), "alice", None);

        let Err(error) = build_session(&user_config_with_proof_key(pem_path)) else {
            panic!("a configured key that is not the leaf key must fail locally");
        };
        assert!(
            error.to_string().contains("does not match"),
            "configured-key mismatch must be actionable: {error}"
        );
    });
}

#[test]
fn unbound_biscuit_without_pop_key_keeps_token_only_behavior() {
    with_isolated_env(|_| {
        store_credential(&mint_unbound_token("legacy-sa"), "legacy-sa", None);

        let session = build_session(&UserConfig::default()).expect("unbound biscuit session");
        assert_token_only(&session);
    });
}

#[test]
fn opaque_unbound_bearer_keeps_token_only_behavior() {
    with_isolated_env(|_| {
        store_credential("not-a-biscuit", "opaque", None);

        let session = build_session(&UserConfig::default()).expect("opaque bearer session");
        assert_token_only(&session);
    });
}

#[test]
fn unauthenticated_session_has_no_bearer() {
    with_isolated_env(|_| {
        let session = build_session(&UserConfig::default()).expect("unauthenticated session");
        assert!(session.client_config().token.is_none());
        assert!(session.client_config().auth_proof_key_pem.is_none());
    });
}
