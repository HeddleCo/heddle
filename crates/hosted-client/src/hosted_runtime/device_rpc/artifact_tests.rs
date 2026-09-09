//! Exact private artifact reads through the real device endpoint.
use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<thread_api::transport::IrohTransport<thread_api::credentials::Credentials>>,
    repository: &repo::Repository,
    spool: uuid::Uuid,
) {
    let runs = repo::device_runs::RunStore::open(repository.heddle_dir()).expect("runs");
    let run = RecordRef { spool: Some(SpoolRef { id: spool.to_string() }), id: "artifact-rpc-test".into() };
    runs.put_run(RunRecord { r#ref: Some(run.clone()), ..Default::default() }).expect("Run");
    let policy = RunPolicy { spool: run.spool.clone(), retain_raw: true, raw_retention_seconds: 60, ..Default::default() };
    runs.put_policy(&PutRunPolicyRequest { client_operation_id: uuid::Uuid::new_v4().to_string(), policy: Some(policy.clone()), ..Default::default() }, "human").expect("opt in");
    let store = repo::device_artifacts::ArtifactStore::open(repository.heddle_dir()).expect("catalog");
    let retained = store.retain(&run, "report", "text/plain", b"private report", chrono::Utc::now().timestamp()).expect("retain");
    let request = ReadArtifactRequest { artifact: retained.r#ref.clone(), offset: 2, length: 5, ..Default::default() };
    let mut stream = remote.api.observe::<thread_api::rpc::ContentServiceReadArtifact>(&request).await.expect("artifact stream");
    let event = stream.next().await.expect("chunk").expect("event");
    assert_eq!(event.artifact, retained.r#ref);
    match event.payload.expect("payload") {
        artifact_event::Payload::Chunk(chunk) => {
            assert_eq!(chunk.data, b"ivate");
            assert_eq!(chunk.offset, 2);
            assert_eq!(chunk.total_size, 14);
            assert_eq!(chunk.object_hash, retained.content_hash);
            assert!(chunk.range_complete);
        }
        other => panic!("expected artifact chunk: {other:?}"),
    }
    assert!(matches!(stream.next().await.expect("completion").expect("event").payload, Some(artifact_event::Payload::Complete(status)) if status.coverage == Coverage::Complete as i32));
    assert!(stream.next().await.expect("EOF").is_none());
    for denied in [
        ReadArtifactRequest { offset: 15, ..request.clone() },
        ReadArtifactRequest { budget: Some(ReadBudget { max_snapshot_bytes: 1, ..Default::default() }), ..request.clone() },
        ReadArtifactRequest { artifact: Some(RecordRef { id: uuid::Uuid::new_v4().to_string(), spool: run.spool.clone() }), ..request.clone() },
    ] {
        let mut stream = remote.api.observe::<thread_api::rpc::ContentServiceReadArtifact>(&denied).await.expect("typed stream opens");
        assert!(stream.next().await.is_err(), "invalid artifact request must fail before a chunk");
    }
    runs.put_policy(&PutRunPolicyRequest { client_operation_id: uuid::Uuid::new_v4().to_string(), policy: Some(RunPolicy { retain_raw: false, ..policy }), ..Default::default() }, "human").expect("withdraw opt in");
    let mut stream = remote.api.observe::<thread_api::rpc::ContentServiceReadArtifact>(&request).await.expect("typed stream opens");
    assert!(stream.next().await.is_err(), "current policy must deny previously retained data");
}
