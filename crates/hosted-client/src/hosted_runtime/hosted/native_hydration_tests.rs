//! Exercise the assembled CLI transport and real local object installation.
use std::net::Ipv4Addr;

use api::{
    framing::{decode_request_frame, encode_stream_message, encode_success_response},
    heddle::api::v2alpha1 as v2,
};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use objects::{
    object::{Attribution, Blob, Principal, State, Tree, TreeEntry},
    store::ObjectStore,
};
use prost::Message;

use super::{CallContextFactory, HostedClient};

async fn exercise(corrupt: bool, truncated: bool) {
    let temp = tempfile::tempdir().expect("repository directory");
    let repo = repo::Repository::init_default(temp.path()).expect("repository");
    let wanted = Blob::from("selected immutable content\n");
    let sibling = Blob::from("another missing blob\n");
    repo.record_missing_blob(wanted.hash())
        .expect("wanted marker");
    repo.record_missing_blob(sibling.hash())
        .expect("sibling marker");
    std::fs::write(temp.path().join("checkout.txt"), "untouched").expect("checkout sentinel");
    let tree = Tree::from_entries(vec![
        TreeEntry::file("selected.txt", wanted.hash(), false).expect("wanted entry"),
        TreeEntry::file("sibling.txt", sibling.hash(), false).expect("sibling entry"),
    ]);
    let root = repo.store().put_tree(&tree).expect("tree");
    let source = State::new(
        root,
        vec![],
        Attribution::human(Principal::new("Test", "test@example.com")),
    );
    repo.store().put_state(&source).expect("exact source State");
    let state = source.id();
    let spool = uuid::Uuid::now_v7().to_string();
    let revision = v2::RevisionRef {
        spool: Some(v2::SpoolRef { id: spool.clone() }),
        revision: Some(v2::revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: state.as_bytes().to_vec(),
            },
        )),
    };
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("server address")
        .bind()
        .await
        .expect("server");
    let address = server.addr();
    let key = server.id().as_bytes().to_vec();
    let hash = wanted.hash().as_bytes().to_vec();
    let payload = if corrupt {
        b"different bytes\n".to_vec()
    } else {
        wanted.content().to_vec()
    };
    let task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("client")
            .await
            .expect("connection");
        let (mut send, mut recv) = connection.accept_bi().await.expect("discovery");
        let request = recv.read_to_end(1024 * 1024).await.expect("request");
        assert_eq!(
            decode_request_frame(&request).expect("frame").method,
            "/heddle.api.v2alpha1.EndpointService/DescribeEndpoint"
        );
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key,
            }),
            supported_packages: vec!["heddle.api.v2alpha1".into()],
            implemented_methods: vec!["/heddle.api.v2alpha1.ContentService/ReadContent".into()],
            default_read_budget: Some(v2::ReadBudget {
                max_items: 16,
                max_frame_bytes: 65536,
                max_snapshot_bytes: 1024 * 1024,
            }),
            max_pending_batch_bytes: 1024 * 1024,
            ..Default::default()
        };
        send.write_all(
            &encode_success_response(&description.encode_to_vec()).expect("description"),
        )
        .await
        .expect("send");
        send.finish().expect("FIN");
        let (mut send, mut recv) = connection.accept_bi().await.expect("exact hash read");
        let request = recv.read_to_end(1024 * 1024).await.expect("request");
        let frame = decode_request_frame(&request).expect("frame");
        assert_eq!(
            frame.method,
            "/heddle.api.v2alpha1.ContentService/ReadContent"
        );
        let read = v2::ReadContentRequest::decode(frame.body).expect("native selection");
        assert_eq!(read.revision, Some(revision.clone()));
        assert_eq!(read.selections.len(), 1, "no sibling or whole-tip request");
        assert_eq!(
            read.selections[0].selection,
            Some(v2::content_read::Selection::Blob(v2::BlobRead {
                source: Some(v2::blob_read::Source::ObjectHash(hash.clone())),
                offset: 0,
                length: 0,
            }))
        );
        let selection = read.selections[0].selection_id.clone();
        let chunk = v2::ContentEvent {
            selection_id: selection.clone(),
            revision: Some(revision.clone()),
            payload: Some(v2::content_event::Payload::Blob(v2::BlobChunk {
                offset: 0,
                total_size: payload.len() as u64,
                data: payload,
                object_hash: hash,
                range_complete: true,
            })),
        };
        send.write_all(&encode_stream_message(&chunk.encode_to_vec()).expect("chunk"))
            .await
            .expect("send chunk");
        if !truncated {
            let complete = v2::ContentEvent {
                selection_id: selection,
                revision: Some(revision.clone()),
                payload: Some(v2::content_event::Payload::SelectionComplete(
                    v2::SectionStatus {
                        section: "blob".into(),
                        computed_for: Some(revision),
                        coverage: v2::Coverage::Complete as i32,
                        ..Default::default()
                    },
                )),
            };
            send.write_all(&encode_stream_message(&complete.encode_to_vec()).expect("completion"))
                .await
                .expect("send completion");
        }
        send.finish().expect("content FIN");
        connection.closed().await;
        server.close().await;
    });
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("client address")
        .bind()
        .await
        .expect("client");
    let client =
        HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
            .await
            .expect("assembled client");
    let result = client
        .hydrate_blob(&repo, &spool, state, wanted.hash())
        .await;
    client.close().await;
    task.await.expect("native server finished");
    if corrupt || truncated {
        assert!(
            result.is_err(),
            "unverified or incomplete content cannot be installed"
        );
        assert!(!repo.store().has_blob(&wanted.hash()).expect("absence"));
        assert!(
            repo.is_missing_blob(&wanted.hash())
                .expect("marker retained")
        );
    } else {
        assert_eq!(result.expect("native hydration"), 1);
        assert_eq!(
            repo.store()
                .get_blob(&wanted.hash())
                .expect("stored blob")
                .expect("present"),
            wanted
        );
        assert!(
            !repo
                .is_missing_blob(&wanted.hash())
                .expect("marker cleared")
        );
    }
    assert!(
        !repo
            .store()
            .has_blob(&sibling.hash())
            .expect("sibling absent")
    );
    assert!(
        repo.is_missing_blob(&sibling.hash())
            .expect("sibling marker unchanged")
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("checkout.txt")).expect("checkout"),
        "untouched"
    );
}

#[tokio::test]
async fn native_hydration_fetches_only_exact_missing_hash() {
    exercise(false, false).await;
}

#[tokio::test]
async fn native_hydration_rejects_content_that_does_not_hash_to_requested_object() {
    exercise(true, false).await;
}

#[tokio::test]
async fn native_hydration_requires_selection_completion_before_installing() {
    exercise(false, true).await;
}
