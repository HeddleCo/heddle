// SPDX-License-Identifier: Apache-2.0
use std::{sync::Arc, time::Instant};

use api::heddle::api::v1alpha1::{
    ListRefsResponse, PullRequest, RefEntry, RepositoryRef, StateId as ProtoStateId,
    repository_ref::Reference,
};
use heddle_iroh_transport_experiment::{ExperimentRepository, IrohClient, IrohServer};
use objects::{
    object::{Blob, StateId},
    store::{FsStore, ObjectStore, PackObjectId},
};
use wire::{ObjectId, ObjectInfo, ObjectType};

#[tokio::test]
async fn established_connection_lists_refs_and_installs_native_pack() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = FsStore::new(source_dir.path());
    let blob = Blob::from("native pack over an Iroh QUIC stream");
    let hash = source.put_blob(&blob).unwrap();
    let state = StateId::from_content_hash(hash);
    let refs = ListRefsResponse {
        head_thread: "main".to_string(),
        head_state: Some(ProtoStateId {
            value: state.as_bytes().to_vec(),
        }),
        refs: vec![RefEntry {
            name: "main".to_string(),
            state_id: Some(ProtoStateId {
                value: state.as_bytes().to_vec(),
            }),
            is_thread: true,
            revision_address: format!("heddle:{}", state.to_string_full()),
        }],
    };
    let repo = ExperimentRepository::new(
        source,
        refs,
        state,
        vec![ObjectInfo {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::Blob,
            size: blob.size() as u64,
            delta_base: None,
        }],
    );
    let server = IrohServer::spawn_loopback(Arc::new(repo)).await.unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let target = FsStore::new(target_dir.path());
    let client = Arc::new(IrohClient::connect_loopback(server.addr()).await.unwrap());

    let listed = client.list_refs("/").await.unwrap();
    assert_eq!(listed.head_thread, "main");
    assert_eq!(listed.refs.len(), 1);

    let wire = client.benchmark_wire(2 * 1024 * 1024 + 17).await.unwrap();
    assert_eq!(wire.bytes, 2 * 1024 * 1024 + 17);

    let outcome = client
        .pull_native(
            &target,
            PullRequest {
                repo_path: Some(RepositoryRef {
                    reference: Some(Reference::CanonicalPath("/".to_string())),
                }),
                target_state: Some(ProtoStateId {
                    value: state.as_bytes().to_vec(),
                }),
                ..PullRequest::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.installed, vec![PackObjectId::Hash(hash)]);
    assert!(outcome.pack_bytes > blob.size());
    assert!(outcome.index_bytes > 0);
    assert_eq!(target.get_blob(&hash).unwrap(), Some(blob));

    let started = Instant::now();
    for _ in 0..25 {
        client.list_refs("/").await.unwrap();
    }
    eprintln!(
        "25 established-connection ListRefs operations: {:?}",
        started.elapsed()
    );

    let mut concurrent = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let client = client.clone();
        concurrent.spawn(async move { client.list_refs("/").await });
    }
    while let Some(result) = concurrent.join_next().await {
        assert!(result.unwrap().is_ok());
    }

    Arc::try_unwrap(client).unwrap().close().await;
    server.shutdown().await.unwrap();
}
