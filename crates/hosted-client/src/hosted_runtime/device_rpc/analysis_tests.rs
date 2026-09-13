//! Real Iroh queued cancellation and analysis publication use actual durable jobs.
use std::sync::Arc;

use objects::{
    object::{
        Attribution, Blob, EntryVisibility, EntryVisibilityEntry, Principal, State, Tree,
        TreeEntry, VisibilityTier,
    },
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
    let hidden_blob = Blob::from(b"pub fn secret() -> i32 { 7 }\n".to_vec());
    repository
        .store()
        .put_blob(&hidden_blob)
        .expect("second Rust source");
    let tree = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::file("answer.rs", blob.hash(), false).expect("visible path"),
            TreeEntry::file("hidden.rs", hidden_blob.hash(), false).expect("hidden path"),
        ],
        vec![[31; 32], [32; 32]],
    )
    .expect("salted analysis tree");
    repository.store().put_tree(&tree).expect("tree");
    let base = repository.head().expect("head").expect("base");
    let state = State::new_snapshot(
        tree.hash(),
        vec![base],
        Attribution::human(Principal::new("owner", "")),
    );
    repository.store().put_state(&state).expect("source");
    let thread = repository
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
        thread: Some(ThreadRef {
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            id: Some(ThreadId {
                value: thread.thread_id().as_bytes().to_vec(),
            }),
        }),
        kinds: vec![AnalysisKind::SemanticIndex as i32],
        execution_endpoint: Some(device.endpoint()),
        ..Default::default()
    };
    let unrelated = repository
        .create_native_thread("analysis-unrelated", base, None, "other Thread")
        .expect("unrelated Thread");
    let mut wrong = request.clone();
    wrong.client_operation_id = uuid::Uuid::new_v4().to_string();
    wrong.thread = Some(ThreadRef {
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        id: Some(ThreadId {
            value: unrelated.thread_id().as_bytes().to_vec(),
        }),
    });
    assert!(
        remote
            .api
            .call::<thread_api::rpc::AnalysisServiceStartAnalysis>(&wrong)
            .await
            .is_err(),
        "same-Spool Thread without this accepted source cannot authorize analysis"
    );
    let wrong_view = remote
        .api
        .observe::<thread_api::rpc::AnalysisServiceObserveAnalysis>(&ObserveAnalysisRequest {
            source: Some(revision.clone()),
            thread: wrong.thread,
            kinds: vec![AnalysisKind::SemanticIndex as i32],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await;
    let rejected = match wrong_view {
        Err(_) => true,
        Ok(mut stream) => {
            let mut leaked = false;
            while let Ok(Some(event)) = stream.next().await {
                leaked |= matches!(
                    event.payload,
                    Some(
                        analysis_event::Payload::Analysis(_) | analysis_event::Payload::Finding(_)
                    )
                );
            }
            !leaked
        }
    };
    assert!(
        rejected,
        "a readable source object is not an analysis capability on another Thread"
    );
    let mut wrong_base = request.clone();
    wrong_base.client_operation_id = uuid::Uuid::new_v4().to_string();
    wrong_base.base = Some(revision.clone());
    wrong_base.base_thread = Some(ThreadRef {
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        id: Some(ThreadId {
            value: unrelated.thread_id().as_bytes().to_vec(),
        }),
    });
    wrong_base.kinds = vec![AnalysisKind::SemanticDiff as i32];
    assert!(
        remote
            .api
            .call::<thread_api::rpc::AnalysisServiceStartAnalysis>(&wrong_base)
            .await
            .is_err(),
        "the comparison Thread must independently admit its selected base"
    );
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
            thread: request.thread.clone(),
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
    let source_search = SearchRequest {
        threads: vec![request.thread.clone().expect("analysis Thread")],
        domains: vec![SearchDomain::SourceContent as i32],
        text: "pub fn".into(),
        page: Some(PageRequest {
            size: 1,
            ..Default::default()
        }),
        mode: search_request::Mode::Lexical as i32,
        ..Default::default()
    };
    let mut first = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&source_search)
        .await
        .expect("indexed source content search");
    let mut first_path = None;
    let mut next = Vec::new();
    while let Some(event) = first.next().await.expect("source first page") {
        match event.payload {
            Some(search_event::Payload::Hit(hit)) => {
                assert_eq!(hit.domain, SearchDomain::SourceContent as i32);
                first_path = Some(hit.location.expect("source location").path);
            }
            Some(search_event::Payload::Complete(status)) => {
                assert_eq!(status.coverage, Coverage::Partial as i32);
                next = status.page.expect("source page").next_page;
            }
            _ => {}
        }
    }
    assert!(
        first_path.is_some() && !next.is_empty(),
        "indexed source candidates paginate privately"
    );
    let mut second_request = source_search.clone();
    second_request.page.as_mut().expect("page").after_page = next.clone();
    for _ in 0..2 {
        let mut second = remote
            .api
            .observe::<thread_api::rpc::SearchServiceSearch>(&second_request)
            .await
            .expect("retry source page");
        let mut path = None;
        while let Some(event) = second.next().await.expect("source second page") {
            if let Some(search_event::Payload::Hit(hit)) = event.payload {
                path = Some(hit.location.expect("source location").path);
            }
        }
        assert!(
            path.is_some() && path != first_path,
            "candidate cursor resumes to the other source file without duplication"
        );
    }
    let mut symbols_search = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            threads: source_search.threads.clone(),
            domains: vec![SearchDomain::SourceSymbol as i32],
            text: "answer".into(),
            mode: search_request::Mode::Lexical as i32,
            ..Default::default()
        })
        .await
        .expect("indexed source symbol search");
    let mut symbol_hits = 0;
    while let Some(event) = symbols_search.next().await.expect("symbol search frame") {
        if let Some(search_event::Payload::Hit(hit)) = event.payload {
            assert_eq!(hit.symbol_name.as_deref(), Some("answer"));
            assert_eq!(hit.location.expect("symbol location").path, "answer.rs");
            symbol_hits += 1;
        }
    }
    assert_eq!(symbol_hits, 1);
    let hidden_index = tree
        .entries()
        .iter()
        .position(|entry| entry.name() == "hidden.rs")
        .expect("hidden entry");
    let sidecar = EntryVisibility::new(
        state.change_id,
        tree.hash(),
        vec![EntryVisibilityEntry {
            tree_id: tree.hash(),
            leaf_hash: tree
                .v4_leaf_hash_at(hidden_index)
                .expect("hidden leaf commitment"),
            tier: VisibilityTier::Private {
                scope_label: "analysis-secret".into(),
            },
        }],
    )
    .expect("entry visibility declaration");
    repository
        .restore_entry_visibility_sidecar(
            &state.change_id,
            Some(sidecar.encode().expect("sidecar bytes")),
        )
        .expect("restrict source entry");
    let mut hidden_search = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&SearchRequest {
            page: None,
            ..source_search.clone()
        })
        .await
        .expect("entry-restricted source search");
    let mut visible_paths = Vec::new();
    while let Some(event) = hidden_search.next().await.expect("restricted search frame") {
        if let Some(search_event::Payload::Hit(hit)) = event.payload {
            visible_paths.push(hit.location.expect("visible location").path);
        }
    }
    assert_eq!(
        visible_paths,
        vec!["answer.rs"],
        "hidden indexed text cannot become a Search hit"
    );
    let mut restricted = remote
        .api
        .observe::<thread_api::rpc::AnalysisServiceObserveAnalysis>(&ObserveAnalysisRequest {
            source: Some(RevisionRef {
                spool: Some(SpoolRef {
                    id: spool.to_string(),
                }),
                revision: Some(revision_ref::Revision::State(
                    api::heddle::api::v1alpha1::StateId {
                        value: state.id().as_bytes().to_vec(),
                    },
                )),
            }),
            thread: request.thread.clone(),
            kinds: vec![AnalysisKind::SemanticIndex as i32],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("restricted analysis observation");
    let mut visible_findings = Vec::new();
    while let Some(event) = restricted.next().await.expect("restricted analysis frame") {
        if let Some(analysis_event::Payload::Finding(finding)) = event.payload {
            visible_findings.push(finding.path);
        }
    }
    assert_eq!(
        visible_findings,
        vec!["answer.rs"],
        "a semantic index attachment cannot reveal a withheld source entry"
    );
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
