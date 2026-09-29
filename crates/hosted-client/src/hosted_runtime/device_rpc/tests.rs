//! Exercise the real native endpoint without any hosted service.
use std::{net::Ipv4Addr, sync::Arc};

use api::heddle::api::v1alpha2::*;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};

use super::*;

mod checkouts;
mod replication;
mod runs;
use crate::hosted_runtime::{
    claim_authorization::StoredClaimAuthorization,
    hosted::claim_protocol::{ClaimProtocol, NATIVE_ALPN},
    root_mint::mint_agent_root,
};

#[tokio::test]
async fn real_device_content_obeys_exact_thread_and_entry_visibility() {
    let _process_env_guard = crate::test_process_env::exclusive().await;
    real_device_roundtrip(true).await;
}

#[tokio::test]
async fn real_device_partial_fetch_preserves_signed_source_until_reference_hydration() {
    let _process_env_guard = crate::test_process_env::exclusive().await;
    real_device_roundtrip_partial().await;
}

async fn real_device_roundtrip_partial() {
    real_device_roundtrip_with_partial(false, true).await;
}

#[tokio::test]
async fn real_device_rpc_captures_without_weft_and_rejects_unowned_authority() {
    let _process_env_guard = crate::test_process_env::exclusive().await;
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
    super::content_tests::roundtrip(&remote, &repository, &device, spool).await;
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
    runs::latest_then_follow(&remote, &repository, spool).await;
    runs::principal_visibility(&remote, &browser, address.clone(), key, &repository, spool).await;
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
        .unwrap_or_else(|| panic!("unauthorized stream is denied"));
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

#[tokio::test]
// `lock_test_env` serializes process-global credential storage for this test.
#[allow(clippy::await_holding_lock)]
async fn scoped_contributor_can_capture_refresh_and_claim_only_its_device_spool() {
    let _process_env_guard = crate::test_process_env::exclusive().await;

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
    repo::device_catalog::set_capability_path(home.path(), spool, "org/device")
        .expect("named device scope");
    let other_local = tempfile::tempdir().expect("other repository");
    let other_repository = repo::Repository::init_default(other_local.path()).expect("other repo");
    let other_base = other_repository
        .head()
        .expect("other head")
        .expect("other base");
    let other_replica = other_repository
        .create_native_thread(
            "other-device-test",
            other_base,
            None,
            "other device operations",
        )
        .expect("other Thread");
    let other_spool = uuid::Uuid::parse_str(&other_replica.genesis().expect("other genesis").spool)
        .expect("other spool UUID");
    repo::device_catalog::register(home.path(), &other_repository, other_spool)
        .expect("other catalog");
    repo::device_catalog::set_capability_path(home.path(), other_spool, "org/other")
        .expect("named other scope");
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

    use crate::hosted_runtime::{auth::derive_agent, credential_file, device_flow::AgentTemplate};
    let parent = mint_agent_root(&[71; 32]).expect("parent root");
    config::credentials::store_server_credential(
        "review-device",
        config::credentials::ServerCredential {
            mint_root_attachment: None,
            token: parent.token,
            subject: parent.subject,
            device_id: None,
            credential_id: None,
            private_key_pem: Some(parent.private_key_pem),
            expires_at: Some(parent.expires_at.to_rfc3339()),
        },
    )
    .expect("store parent");
    let out = home.path().join("contributor.hcred");
    derive_agent(
        "review-device",
        Some("scoped-contributor".into()),
        900,
        vec!["spool:org/device".to_string()],
        Vec::new(),
        Some(AgentTemplate::Contributor),
        Some(&out),
    )
    .expect("derive scoped contributor");
    let child = credential_file::load_credential_file(&out).expect("child credential");
    let transport = thread_api::transport::IrohTransport::new(
        browser
            .connect(address.clone(), NATIVE_ALPN)
            .await
            .expect("child Iroh"),
        thread_api::credentials::Credentials::Signed {
            signer: Arc::new(Ed25519Signer::from_pem(&child.proof_key_pem).expect("child key")),
            biscuit: child.token.into_bytes(),
            grant_envelope: Vec::new(),
        },
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("child transport");
    let child = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("child discovery");
    let resolve = |id| ResolveResourcesRequest {
        selectors: vec![ResourceSelector {
            selector: Some(resource_selector::Selector::Resource(EntityRef {
                entity: Some(entity_ref::Entity::Spool(SpoolRef { id })),
            })),
        }],
        budget: None,
    };
    let own = child
        .api
        .call::<thread_api::rpc::WorkspaceServiceResolveResources>(&resolve(spool.to_string()))
        .await
        .expect("scoped contributor resolves its own spool");
    assert_eq!(own.results[0].coverage, Coverage::Complete as i32);
    let other = child
        .api
        .call::<thread_api::rpc::WorkspaceServiceResolveResources>(&resolve(
            other_spool.to_string(),
        ))
        .await
        .expect("out-of-scope resolution stays private");
    assert_eq!(other.results[0].coverage, Coverage::Unavailable as i32);
    let spool_ref = SpoolRef {
        id: spool.to_string(),
    };
    let source = RevisionRef {
        spool: Some(spool_ref.clone()),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::common::StateId {
                value: base.as_bytes().to_vec(),
            },
        )),
    };
    let overview = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceMaterialize>(&MaterializeCheckoutRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            thread: Some(ThreadRef {
                spool: Some(spool_ref),
                id: Some(ThreadId {
                    value: replica.thread_id().as_bytes().to_vec(),
                }),
            }),
            revision: Some(source.clone()),
            ..Default::default()
        })
        .await
        .expect("parent creates agent lane checkout")
        .checkout
        .expect("overview");
    let claim = ClaimCheckoutWriterRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: overview.r#ref.clone(),
        expected_checkout_version: overview.version.clone(),
        ..Default::default()
    };
    let mut other_claim = claim.clone();
    other_claim.checkout.as_mut().expect("checkout").spool = Some(SpoolRef {
        id: other_spool.to_string(),
    });
    let denied = child
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&other_claim)
        .await;
    assert!(
        matches!(
            &denied,
            Err(api::v2::client::ClientError::Transport(
                thread_api::transport::Error::Remote(failure)
            )) if failure.code == CallFailureCode::Unauthenticated as i32
        ),
        "other spool claim: {denied:?}"
    );
    let lease = child
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&claim)
        .await
        .expect("scoped claim")
        .lease
        .expect("lease");
    std::fs::write(
        std::path::Path::new(&overview.display_path).join("agent.txt"),
        "derived capture",
    )
    .expect("edit");
    let capture = CaptureCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: overview.r#ref.clone(),
        expected_checkout_version: overview.version.clone(),
        expected_source: Some(source.clone()),
        writer_lease_token: lease.token.clone(),
        summary: "review capture".into(),
        ..Default::default()
    };
    let mut other_capture = capture.clone();
    other_capture.checkout.as_mut().expect("checkout").spool = Some(SpoolRef {
        id: other_spool.to_string(),
    });
    let denied = child
        .api
        .call::<thread_api::rpc::CheckoutServiceCapture>(&other_capture)
        .await;
    assert!(
        matches!(
            &denied,
            Err(api::v2::client::ClientError::Transport(
                thread_api::transport::Error::Remote(failure)
            )) if failure.code == CallFailureCode::Unauthenticated as i32
        ),
        "other spool capture: {denied:?}"
    );
    let captured = child
        .api
        .call::<thread_api::rpc::CheckoutServiceCapture>(&capture)
        .await
        .expect("scoped capture")
        .checkout
        .expect("captured");
    let refresh = RefreshCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: captured.r#ref,
        expected_checkout_version: captured.version,
        expected_source: captured.materialized,
        target: Some(source),
        writer_lease_token: lease.token,
    };
    let mut other_refresh = refresh.clone();
    other_refresh.checkout.as_mut().expect("checkout").spool = Some(SpoolRef {
        id: other_spool.to_string(),
    });
    let denied = child
        .api
        .call::<thread_api::rpc::CheckoutServiceRefresh>(&other_refresh)
        .await;
    assert!(
        matches!(
            &denied,
            Err(api::v2::client::ClientError::Transport(
                thread_api::transport::Error::Remote(failure)
            )) if failure.code == CallFailureCode::Unauthenticated as i32
        ),
        "other spool refresh: {denied:?}"
    );
    child
        .api
        .call::<thread_api::rpc::CheckoutServiceRefresh>(&refresh)
        .await
        .expect("scoped refresh");
    drop(child);
    drop(remote);
    browser.close().await;
    router.shutdown().await.expect("router shutdown");
}
