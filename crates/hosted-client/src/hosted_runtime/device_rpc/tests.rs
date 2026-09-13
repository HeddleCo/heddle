//! Exercise the real native endpoint without any hosted service.
use std::{net::Ipv4Addr, sync::Arc};

use api::heddle::api::v2alpha1::*;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};

use super::*;

mod checkouts;
mod replication;
use crate::hosted_runtime::{
    claim_authorization::StoredClaimAuthorization,
    hosted::claim_protocol::{ClaimProtocol, NATIVE_ALPN},
    root_mint::mint_agent_root,
};

#[tokio::test]
async fn real_device_content_obeys_exact_thread_and_entry_visibility() {
    real_device_roundtrip(true).await;
}

#[tokio::test]
async fn real_device_partial_fetch_preserves_signed_source_until_reference_hydration() {
    real_device_roundtrip_partial().await;
}

async fn real_device_roundtrip_partial() {
    real_device_roundtrip_with_partial(false, true).await;
}

#[tokio::test]
async fn real_device_rpc_captures_without_weft_and_rejects_unowned_authority() {
    real_device_roundtrip(false).await;
}

// reason: `lock_test_env` serializes process-global HEDDLE_HOME/credential
// mutation, so the guard is deliberately held across the whole async scenario
// (payload `()`, single per-test runtime — no other task contends, no deadlock).
#[allow(clippy::await_holding_lock)]
async fn real_device_roundtrip(content_only: bool) {
    real_device_roundtrip_with_partial(content_only, false).await;
}

#[allow(clippy::await_holding_lock)]
async fn real_device_roundtrip_with_partial(content_only: bool, partial_only: bool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
        .with_test_writer()
        .try_init();
    let _guard = config::credentials::lock_test_env();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(value) => std::env::set_var("HEDDLE_HOME", value),
                    None => std::env::remove_var("HEDDLE_HOME"),
                }
            }
        }
    }
    let home = tempfile::tempdir().expect("home");
    let _restore = Restore(std::env::var_os("HEDDLE_HOME"));
    unsafe {
        std::env::set_var("HEDDLE_HOME", home.path());
    }

    let local = tempfile::tempdir().expect("repository");
    let repository = repo::Repository::init_default(local.path()).expect("repo");
    let root = Ed25519Signer::from_seed(&[71; 32]).expect("root");
    let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery");
    let signed =
        repo::sign_custodial_owner_root(&root, &recovery, [9; 16], [5; 32]).expect("owner root");
    let binding = repo::sign_custodial_owner_binding(&root, &signed, [6; 32]).expect("binding");
    let verified = heddleco_capability_verifier::verify_owner_root(&signed).expect("root proof");
    let owner = OwnerState {
        owner: Some(PrincipalRef {
            id: uuid::Uuid::from_bytes([9; 16]).to_string(),
        }),
        root: Some(signed),
        binding: Some(binding),
        version: verified.state_hash().to_vec(),
        ..Default::default()
    };
    repo::device_authority::publish(
        home.path(),
        &repo::device_authority::DeviceAuthority {
            owner: owner.clone(),
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        },
        chrono::Utc::now().timestamp(),
    )
    .expect("independent local enrollment");
    let base = repository.head().expect("head").expect("base");
    let replica = repository
        .create_native_thread("device-test", base, None, "device operations")
        .expect("Thread");
    let spool = uuid::Uuid::parse_str(&replica.genesis().expect("genesis").spool).expect("spool");
    repo::device_catalog::register(home.path(), &repository, spool).expect("catalog");
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("address")
        .bind()
        .await
        .expect("endpoint");
    let address = endpoint.addr();
    let key = *endpoint.id().as_bytes();
    let device = Arc::new(DeviceRpc::new(home.path().to_owned(), key));
    let (authorization, _, _) = StoredClaimAuthorization::new();
    let authorization = Arc::new(authorization);
    let protocol =
        ClaimProtocol::new(authorization.clone(), authorization, key).with_device(device.clone());
    let budgets = protocol.budgets();
    let endpoint_signer =
        Ed25519Signer::from_seed(&endpoint.secret_key().to_bytes()).expect("actual endpoint key");
    let router = Router::builder(endpoint)
        .accept(NATIVE_ALPN, protocol)
        .spawn();
    let browser = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("browser address")
        .bind()
        .await
        .expect("browser");
    let connection = browser
        .connect(address.clone(), NATIVE_ALPN)
        .await
        .expect("direct Iroh");
    let token = mint_agent_root(&[71; 32]).expect("self issued root").token;
    let credentials = thread_api::credentials::Credentials::Signed {
        signer: Arc::new(root),
        biscuit: token.into_bytes(),
        grant_envelope: Vec::new(),
    };
    let transport = thread_api::transport::IrohTransport::new(
        connection,
        credentials,
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("transport");
    let remote = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("discover");
    super::fetch_tests::initial_base_roundtrip(&remote, &replica).await;
    if partial_only {
        super::fetch_tests::partial_roundtrip(
            &remote,
            &repository,
            &replica,
            &endpoint_signer,
            &owner,
        )
        .await;
        drop(remote);
        browser.close().await;
        router.shutdown().await.expect("router shutdown");
        return;
    }
    super::artifact_tests::roundtrip(&remote, &repository, spool).await;
    super::content_tests::roundtrip(&remote, &repository, spool).await;
    if content_only {
        drop(remote);
        browser.close().await;
        router.shutdown().await.expect("router shutdown");
        return;
    }
    super::publication_tests::roundtrip(&remote, &repository, *browser.id().as_bytes()).await;
    super::collaboration_tests::roundtrip(&remote, &repository, &replica, spool).await;
    super::evidence_tests::roundtrip(&remote, &device, &repository, &replica, spool).await;
    #[cfg(feature = "semantic")]
    super::analysis_tests::roundtrip(&remote, &device, &repository, spool).await;
    super::thread_tests::roundtrip(&remote, &device, &repository, spool).await;
    super::account_tests::roundtrip(&remote, &device, spool).await;
    super::sibling_tests::roundtrip(
        home.path(),
        &browser,
        address.clone(),
        key,
        &owner,
        &repository,
        spool,
    )
    .await;
    super::capacity_tests::roundtrip(&remote, &device, &repository, &replica, &budgets).await;
    super::receipt_tests::roundtrip(
        &remote,
        &device,
        &repository,
        &replica,
        *browser.id().as_bytes(),
    )
    .await;
    let (materialize, target_replica) =
        checkouts::roundtrip(&remote, &device, &repository, &replica, base, spool).await;
    super::ownership_tests::claim(&remote, &repository, &replica).await;
    super::fetch_tests::claimed_roundtrip(&remote, &repository, &replica, &endpoint_signer, &owner)
        .await;
    super::fetch_tests::roundtrip(
        &remote,
        &repository,
        &target_replica,
        &endpoint_signer,
        &owner,
    )
    .await;
    replication::roundtrip(
        &remote,
        &device,
        &repository,
        &replica,
        home.path(),
        &browser,
        base,
    )
    .await;
    let wrong = mint_agent_root(&[73; 32]).expect("unowned root");
    let transport = thread_api::transport::IrohTransport::new(
        browser
            .connect(address, NATIVE_ALPN)
            .await
            .expect("connection"),
        thread_api::credentials::Credentials::Signed {
            signer: Arc::new(Ed25519Signer::from_seed(&[73; 32]).expect("other key")),
            biscuit: wrong.token.into_bytes(),
            grant_envelope: Vec::new(),
        },
        256 * 1024,
        std::time::Duration::from_secs(5),
    )
    .expect("transport");
    let other = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("public description");
    assert!(
        other
            .api
            .call::<thread_api::rpc::CheckoutServiceMaterialize>(&MaterializeCheckoutRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                ..materialize
            })
            .await
            .is_err(),
        "self-minted unrelated authority cannot control this device"
    );
    drop(other);
    drop(remote);
    browser.close().await;
    router.shutdown().await.expect("router shutdown");
}

pub(super) async fn denied_spool_stream_is_typed(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    spool: uuid::Uuid,
) {
    let mut denied = remote
        .observe::<thread_api::rpc::CheckoutServiceObserveCheckouts>(
            ObserveCheckoutsRequest {
                spool: Some(SpoolRef {
                    id: spool.to_string(),
                }),
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("denied stream opens its framed response");
    let error = denied
        .next_commit()
        .await
        .err()
        .expect("unauthorized stream is denied");
    match error {
        thread_api::observation::Error::Client(api::v2::client::ClientError::Transport(
            thread_api::transport::Error::Remote(failure),
        )) => assert_eq!(
            failure.code,
            CallFailureCode::Unauthenticated as i32,
            "denied exact-Spool stream preserves typed authorization failure"
        ),
        other => {
            panic!("denied exact-Spool stream must preserve typed authorization failure: {other}")
        }
    }
}
