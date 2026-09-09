//! Exercise the real native endpoint without any hosted service.
use std::{net::Ipv4Addr, sync::Arc};

use api::heddle::api::v2alpha1::*;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};

use super::*;
use crate::hosted_runtime::{
    claim_authorization::StoredClaimAuthorization,
    hosted::claim_protocol::{ClaimProtocol, NATIVE_ALPN},
    root_mint::mint_agent_root,
};

#[tokio::test]
async fn real_device_rpc_captures_without_weft_and_rejects_unowned_authority() {
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
        &owner,
        &[],
        &[],
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
    let router = Router::builder(endpoint)
        .accept(
            NATIVE_ALPN,
            ClaimProtocol::new(authorization.clone(), authorization, key)
                .with_device(device.clone()),
        )
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
    let spool_ref = SpoolRef {
        id: spool.to_string(),
    };
    let source = RevisionRef {
        spool: Some(spool_ref.clone()),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: base.as_bytes().to_vec(),
            },
        )),
    };
    let materialize = MaterializeCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        thread: Some(ThreadRef {
            spool: Some(spool_ref.clone()),
            id: Some(ThreadId {
                value: replica.thread_id().as_bytes().to_vec(),
            }),
        }),
        revision: Some(source.clone()),
        ..Default::default()
    };
    let result = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceMaterialize>(&materialize)
        .await
        .expect("materialize offline");
    let overview = result.checkout.expect("checkout overview");
    let checkout = overview.r#ref.clone().expect("checkout ref");
    let claimed = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&ClaimCheckoutWriterRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: Some(checkout.clone()),
            expected_checkout_version: overview.version.clone(),
            ..Default::default()
        })
        .await
        .expect("writer");
    let lease = claimed.lease.expect("lease");
    let conflicting = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&ClaimCheckoutWriterRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: Some(checkout.clone()),
            expected_checkout_version: overview.version.clone(),
            ..Default::default()
        })
        .await;
    assert!(
        conflicting.is_err(),
        "a second writer cannot claim the same physical checkout"
    );
    let mut observed = remote
        .observe::<thread_api::rpc::CheckoutServiceObserveCheckouts>(
            ObserveCheckoutsRequest {
                spool: Some(spool_ref.clone()),
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Follow as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("live checkout view");
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), observed.next_commit())
        .await
        .expect("snapshot deadline")
        .expect("snapshot protocol")
        .expect("snapshot");
    assert!(snapshot.replace);
    assert_eq!(snapshot.changes.len(), 1);
    let idle_feed = device
        .feeds
        .lock()
        .expect("feeds")
        .get(&spool)
        .expect("active feed")
        .upgrade()
        .expect("feed retained");
    let idle_generation = *idle_feed.changes.borrow();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(1100),
            observed.next_commit()
        )
        .await
        .is_err(),
        "idle view emits no polling frames"
    );
    assert_eq!(
        *idle_feed.changes.borrow(),
        idle_generation,
        "idle SQL reads must not manufacture OS change events"
    );
    drop(idle_feed);
    std::fs::write(
        std::path::Path::new(&overview.display_path).join("hello.txt"),
        "native capture",
    )
    .expect("working edit");
    let request = CaptureCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: Some(checkout),
        expected_checkout_version: overview.version,
        expected_source: Some(source),
        writer_lease_token: lease.token,
        summary: "device capture".into(),
        ..Default::default()
    };
    let captured = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceCapture>(&request)
        .await
        .expect("native capture");
    let changed = tokio::time::timeout(std::time::Duration::from_secs(5), observed.next_commit())
        .await
        .expect("filesystem/source update wakes live view");
    assert!(
        changed.is_ok() || matches!(changed, Err(thread_api::observation::Error::Reset(_))),
        "update either commits or explicitly resets a racing snapshot"
    );
    drop(observed);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let released = device
                .feeds
                .lock()
                .expect("feeds")
                .get(&spool)
                .is_none_or(|feed| feed.strong_count() == 0);
            if released {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation drops the observer and OS watcher");
    let retry = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceCapture>(&request)
        .await
        .expect("fresh proof exact retry");
    assert_eq!(
        captured, retry,
        "retries return the original durable receipt"
    );
    assert_eq!(replica.view().expect("source").source_heads.len(), 1);
    let target_replica = repository
        .create_native_thread("device-target", base, None, "local landing")
        .expect("target Thread");
    let captured_overview = captured.checkout.as_ref().expect("captured checkout");
    let policy = captured_overview
        .actions
        .iter()
        .find(|action| action.method.ends_with("/LandCheckout"))
        .expect("landing action")
        .observed_versions[0]
        .version
        .clone();
    let landing = LandCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: captured_overview.r#ref.clone(),
        expected_checkout_version: captured_overview.version.clone(),
        source: captured_overview.materialized.clone(),
        target: Some(ThreadRef {
            spool: Some(spool_ref.clone()),
            id: Some(ThreadId {
                value: target_replica.thread_id().as_bytes().to_vec(),
            }),
        }),
        expected_target: materialize.revision.clone(),
        expected_policy_version: policy,
        writer_lease_token: request.writer_lease_token.clone(),
    };
    let landed = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceLandCheckout>(&landing)
        .await
        .expect("actual local merge landing");
    assert_eq!(
        target_replica
            .view()
            .expect("target view")
            .source_heads
            .len(),
        1
    );
    assert_eq!(
        landed
            .checkout
            .as_ref()
            .expect("source checkout")
            .materialized,
        captured_overview.materialized,
        "landing advances target Thread without rewriting source checkout"
    );
    assert_eq!(
        landed,
        remote
            .api
            .call::<thread_api::rpc::CheckoutServiceLandCheckout>(&landing)
            .await
            .expect("landing retry")
    );
    let second = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceMaterialize>(&MaterializeCheckoutRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            ..materialize.clone()
        })
        .await
        .expect("second independent checkout")
        .checkout
        .expect("overview");
    let second_writer = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&ClaimCheckoutWriterRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: second.r#ref.clone(),
            expected_checkout_version: second.version.clone(),
            ..Default::default()
        })
        .await
        .expect("separate physical checkout has independent writer")
        .lease
        .expect("writer");
    let refreshed = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceRefresh>(&RefreshCheckoutRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: second.r#ref.clone(),
            expected_checkout_version: second.version,
            expected_source: second.materialized,
            target: captured_overview.materialized.clone(),
            writer_lease_token: second_writer.token.clone(),
        })
        .await
        .expect("refresh admitted source");
    let refreshed = refreshed.checkout.expect("refreshed checkout");
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&refreshed.display_path).join("hello.txt"))
            .expect("refreshed content"),
        "native capture"
    );
    let recover = RecoverCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: refreshed.r#ref.clone(),
        expected_checkout_version: refreshed.version.clone(),
        recover_to: materialize.revision.clone(),
        writer_lease_token: second_writer.token.clone(),
    };
    let recovered = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceRecover>(&recover)
        .await
        .expect("recover prior source into working files");
    assert_eq!(
        recovered
            .checkout
            .as_ref()
            .expect("recovered checkout")
            .materialized,
        refreshed.materialized,
        "recovery preserves recorded source"
    );
    assert!(
        !std::path::Path::new(&refreshed.display_path)
            .join("hello.txt")
            .exists()
    );
    assert_eq!(
        recovered,
        remote
            .api
            .call::<thread_api::rpc::CheckoutServiceRecover>(&recover)
            .await
            .expect("recovery retry")
    );
    remote
        .api
        .call::<thread_api::rpc::CheckoutServiceReleaseCheckoutWriter>(
            &ReleaseCheckoutWriterRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                checkout: refreshed.r#ref,
                token: second_writer.token,
            },
        )
        .await
        .expect("release writer");
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
