//! Real Iroh queued cancellation and analysis publication use actual durable jobs.
use std::sync::Arc;

use objects::{
    object::{Attribution, Blob, Principal, State, Tree, TreeEntry},
    store::ObjectStore,
};

use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    device: &DeviceRpc,
    repository: &repo::Repository,
    spool: uuid::Uuid,
) {
    let blob = Blob::from(b"pub fn answer() -> i32 { 42 }\n".to_vec());
    repository.store().put_blob(&blob).expect("Rust source");
    let mut tree = Tree::new();
    tree.insert(TreeEntry::file("answer.rs", blob.hash(), false).expect("path"));
    repository.store().put_tree(&tree).expect("tree");
    let base = repository.head().expect("head").expect("base");
    let state = State::new_snapshot(
        tree.hash(),
        vec![base],
        Attribution::human(Principal::new("owner", "")),
    );
    repository.store().put_state(&state).expect("source");
    repository
        .create_native_thread("analysis-source", base, None, "analysis fixture")
        .expect("Thread");
    repository
        .record_native_capture("analysis-source", state.id())
        .expect("accepted source");
    let revision = RevisionRef {
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: state.id().as_bytes().to_vec(),
            },
        )),
    };
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *device.analysis.gate.lock().expect("test executor gate") = Some(gate.clone());
    let request = StartAnalysisRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        source: Some(revision.clone()),
        kinds: vec![AnalysisKind::SemanticIndex as i32],
        execution_endpoint: Some(device.endpoint()),
        ..Default::default()
    };
    let response = remote
        .api
        .call::<thread_api::rpc::AnalysisServiceStartAnalysis>(&request)
        .await
        .expect("durable queue acceptance");
    let Some(mutation_receipt::Outcome::PendingOperation(operation)) = response
        .receipt
        .as_ref()
        .and_then(|receipt| receipt.outcome.clone())
    else {
        panic!("pending analysis operation")
    };
    let mut observed = remote
        .api
        .observe::<thread_api::rpc::OperationServiceObserveOperations>(&ObserveOperationsRequest {
            operations: vec![operation.clone()],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Follow as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("follow execution");
    let queued = next_state(&mut observed, operation_record::State::Queued).await;
    remote
        .api
        .call::<thread_api::rpc::OperationServiceCancelOperation>(&CancelOperationRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            operation: Some(operation.clone()),
            expected_version: queued.version,
        })
        .await
        .expect("request cancellation");
    let requested = next_state(&mut observed, operation_record::State::Queued).await;
    assert!(
        requested.cancellation_requested,
        "server pushes committed cancellation request"
    );
    assert!(
        requested.cancellation_supported,
        "request is not execution acknowledgement"
    );
    gate.add_permits(1);
    let cancelled = next_state(&mut observed, operation_record::State::Canceled).await;
    assert!(!cancelled.cancellation_supported);
    drop(observed);
    *device.analysis.gate.lock().expect("gate") = None;
    let busy = device
        .analysis
        .workers
        .clone()
        .acquire_many_owned(2)
        .await
        .expect("hold worker capacity");
    assert_eq!(
        remote
            .api
            .call::<thread_api::rpc::AnalysisServiceStartAnalysis>(&request)
            .await
            .expect("exact retry independent of worker capacity"),
        response
    );
    drop(busy);
    let mut request = request;
    request.client_operation_id = uuid::Uuid::new_v4().to_string();
    let response = remote
        .api
        .call::<thread_api::rpc::AnalysisServiceStartAnalysis>(&request)
        .await
        .expect("execute semantic analysis");
    let Some(mutation_receipt::Outcome::PendingOperation(operation)) =
        response.receipt.and_then(|receipt| receipt.outcome)
    else {
        panic!("pending operation")
    };
    let mut observed = remote
        .api
        .observe::<thread_api::rpc::OperationServiceObserveOperations>(&ObserveOperationsRequest {
            operations: vec![operation],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Follow as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("completion stream");
    next_state(&mut observed, operation_record::State::Completed).await;
    drop(observed);
    let mut analysis = remote
        .api
        .observe::<thread_api::rpc::AnalysisServiceObserveAnalysis>(&ObserveAnalysisRequest {
            source: Some(revision),
            kinds: vec![AnalysisKind::SemanticIndex as i32],
            symbols: vec!["answer".into()],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("query retained semantics");
    let mut symbols = 0;
    let mut record = false;
    while let Some(event) = analysis.next().await.expect("analysis frame") {
        match event.payload {
            Some(analysis_event::Payload::Analysis(value)) => {
                assert_eq!(value.coverage, Coverage::Complete as i32);
                assert_eq!(value.finding_count, Some(1));
                record = true;
            }
            Some(analysis_event::Payload::Finding(value)) => {
                assert_eq!(value.path, "answer.rs");
                assert_eq!(value.line, Some(1));
                assert!(value.explanation.contains("answer"));
                symbols += 1;
            }
            _ => {}
        }
    }
    assert!(record);
    assert_eq!(symbols, 1, "actual indexed symbol crosses Iroh projection");
    let released = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        device.analysis.workers.clone().acquire_many_owned(2),
    )
    .await
    .expect("workers release capacity")
    .expect("semaphore");
    drop(released);
}
async fn next_state<R: api::v2::client::MessageReader>(
    stream: &mut api::v2::client::Messages<R, OperationEvent>,
    wanted: operation_record::State,
) -> OperationRecord {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = stream
                .next()
                .await
                .expect("operation frame")
                .expect("live operation stream");
            if let Some(operation_event::Payload::Operation(record)) = event.payload {
                if record.state == wanted as i32 {
                    return record;
                }
                assert_ne!(
                    record.state,
                    operation_record::State::Failed as i32,
                    "executor failure: {:?}",
                    record.failure
                );
            }
        }
    })
    .await
    .expect("event-driven execution state deadline")
}
