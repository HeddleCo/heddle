//! Real checkout operations share the scenario endpoint, but have a separate
//! async frame from replication and download verification.
use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    device: &DeviceRpc,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    base: objects::object::StateId,
    spool: uuid::Uuid,
) -> (
    MaterializeCheckoutRequest,
    repo::thread_replication::ThreadReplica,
) {
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
    let logical_target = repository
        .create_native_thread("device-logical-target", base, None, "Thread landing")
        .expect("logical target Thread");
    let logical_target_ref = ThreadRef {
        spool: Some(spool_ref.clone()),
        id: Some(ThreadId {
            value: logical_target.thread_id().as_bytes().to_vec(),
        }),
    };
    let mut landing_view = remote
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(
            ObserveThreadRequest {
                thread: materialize.thread.clone(),
                source: captured_overview.materialized.clone(),
                landing_target: Some(logical_target_ref.clone()),
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("target-bound Thread landing view");
    let landing_snapshot = landing_view
        .next_commit()
        .await
        .expect("landing view protocol")
        .expect("landing view snapshot");
    let landing_overview = landing_snapshot
        .changes
        .iter()
        .find_map(|change| match change {
            thread_event::Payload::Overview(value) => Some(value),
            _ => None,
        })
        .expect("landing overview");
    let assessment = landing_overview
        .landing_assessment
        .as_ref()
        .expect("exact source and target assessment");
    assert_eq!(assessment.readiness, ReviewReadiness::Eligible as i32);
    assert!(
        landing_overview
            .actions
            .iter()
            .any(|action| action.method.ends_with("/LandThread")
                && action.authorized
                && action.implemented)
    );
    let logical = LandThreadRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        thread: materialize.thread.clone(),
        source: assessment.source.clone(),
        target: assessment.target.clone(),
        expected_target: assessment.expected_target.clone(),
        expected_policy_version: assessment.policy_version.clone(),
    };
    let first_logical = remote
        .api
        .call::<thread_api::rpc::ThreadServiceLandThread>(&logical)
        .await
        .expect("native Thread landing");
    assert!(
        first_logical.receipt.is_some(),
        "landing has a durable receipt"
    );
    assert_eq!(
        logical_target
            .view()
            .expect("logical target view")
            .source_heads
            .len(),
        1
    );
    assert_eq!(
        first_logical,
        remote
            .api
            .call::<thread_api::rpc::ThreadServiceLandThread>(&logical)
            .await
            .expect("native Thread landing exact retry"),
        "retry returns the original receipt after the target advances"
    );
    let mut changed_logical = logical.clone();
    changed_logical.expected_target = captured_overview.materialized.clone();
    assert!(
        remote
            .api
            .call::<thread_api::rpc::ThreadServiceLandThread>(&changed_logical)
            .await
            .is_err(),
        "same operation ID cannot be rebound to a different comparison"
    );
    let stack_targets = ["device-stack-first", "device-stack-second"]
        .into_iter()
        .map(|name| {
            repository
                .create_native_thread(name, base, None, "Stack target")
                .expect("stack target Thread")
        })
        .collect::<Vec<_>>();
    let stack = LandStackRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        spool: Some(spool_ref.clone()),
        landings: stack_targets
            .iter()
            .map(|target| StackLanding {
                thread: materialize.thread.clone(),
                source: captured_overview.materialized.clone(),
                target: Some(ThreadRef {
                    spool: Some(spool_ref.clone()),
                    id: Some(ThreadId {
                        value: target.thread_id().as_bytes().to_vec(),
                    }),
                }),
                expected_target: materialize.revision.clone(),
                expected_policy_version: logical.expected_policy_version.clone(),
            })
            .collect(),
    };
    let stack_result = remote
        .api
        .call::<thread_api::rpc::ThreadServiceLandStack>(&stack)
        .await
        .expect("native atomic Thread stack");
    assert!(stack_result.receipt.is_some());
    assert_eq!(
        stack_result,
        remote
            .api
            .call::<thread_api::rpc::ThreadServiceLandStack>(&stack)
            .await
            .expect("exact stack retry")
    );
    assert!(
        stack_targets
            .iter()
            .all(|target| target.view().expect("stack view").source_heads.len() == 1)
    );
    let mut changed_stack = stack.clone();
    changed_stack.landings.pop();
    assert!(
        remote
            .api
            .call::<thread_api::rpc::ThreadServiceLandStack>(&changed_stack)
            .await
            .is_err(),
        "stack command ID cannot be rebound to fewer members"
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
    (materialize, target_replica)
}
