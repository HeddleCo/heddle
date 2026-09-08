use std::{ffi::OsString, sync::MutexGuard};

use config::credentials;
use crypto::{Ed25519Signer, Signer as _};
use repo::seq0_authority_public_key;
use tempfile::TempDir;

use super::{
    auth_login_agent::{finish_invite_create_from_response, provision_response_for_test},
    hosted::{CallContextFactory, HostedError},
    identity_state,
    owner_root::load_recorded_root,
};

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
fn invite_create_mints_a_claimable_seq0_root_on_the_agent_proof_key() {
    let _home = IsolatedHome::new();
    let server = "api.owner-root.test";
    let output = finish_invite_create_from_response(
        server,
        provision_response_for_test("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28", "quiet-otter", ""),
    )
    .expect("invite create");
    let state = identity_state::load()
        .expect("load")
        .expect("claim state stored");
    let signed = load_recorded_root(&state)
        .expect("load root")
        .expect("claimable root minted on signup");
    let root = signed.root.as_ref().expect("body");
    assert!(root.claimable_deferred_human);
    assert_eq!(root.format_version, 1);
    assert_eq!(
        seq0_authority_public_key(&signed).expect("seq-0"),
        hex::decode(&state.node_id).expect("node id").as_slice()
    );
    assert_eq!(
        state.seq0_public_key().expect("recorded seq-0"),
        hex::decode(&state.node_id).expect("node id")
    );
    assert_eq!(output.next.command, "heddle claim");
}

#[test]
fn create_spool_genesis_refuses_a_key_that_is_not_seq0() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.seq0-mismatch.test",
        provision_response_for_test("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28", "quiet-otter", ""),
    )
    .expect("signup");
    let other = Ed25519Signer::generate().expect("other key");
    let factory = CallContextFactory::default()
        .with_signing_key_pem(&other.to_pem().expect("pem"), "principal:other")
        .expect("factory");
    let error = factory
        .mint_spool_owner_genesis(
            uuid::Uuid::now_v7(),
            &observed_owner(
                &Ed25519Signer::from_pem(
                    &credentials::get_server_credential("api.seq0-mismatch.test")
                        .expect("credential")
                        .expect("stored")
                        .private_key_pem
                        .expect("key"),
                )
                .expect("owner"),
            ),
        )
        .expect_err("CreateSpool must refuse a genesis key that is not seq-0");
    assert!(
        matches!(error, HostedError::Framing(ref message) if message.contains("current owner authority")),
        "{error}"
    );
}

#[test]
fn create_spool_genesis_matches_the_stored_seq0_proof_key() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.seq0-match.test",
        provision_response_for_test("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28", "quiet-otter", ""),
    )
    .expect("signup");
    let pem = credentials::get_server_credential("api.seq0-match.test")
        .expect("cred")
        .expect("stored")
        .private_key_pem
        .expect("proof pem");
    let signer = Ed25519Signer::from_pem(&pem).expect("agent");
    let factory = CallContextFactory::default()
        .with_signing_key_pem(&pem, "principal:agent")
        .expect("factory");
    let genesis = factory
        .mint_spool_owner_genesis(uuid::Uuid::now_v7(), &observed_owner(&signer))
        .expect("same proof key as seq-0");
    assert_eq!(
        genesis
            .genesis
            .expect("body")
            .owner_public_key
            .expect("key")
            .public_key,
        signer.public_key()
    );
}

pub(crate) fn observed_owner(signer: &Ed25519Signer) -> api::heddle::api::v2alpha1::OwnerState {
    use api::heddle::api::v2alpha1::*;
    let account = uuid::Uuid::new_v4();
    let root = repo::sign_claimable_deferred_human_root(
        signer,
        *account.as_bytes(),
        [1; 32],
        chrono::Utc::now().timestamp(),
    )
    .expect("owner root");
    let state = heddleco_capability_verifier::verify_owner_root(&root).expect("verified root");
    OwnerState {
        owner: Some(PrincipalRef {
            id: account.to_string(),
        }),
        root: Some(root),
        version: state.state_hash().to_vec(),
        ..Default::default()
    }
}
