//! Private source reads use the production Iroh endpoint, exact states and typed failures.
use objects::{
    object::{Attribution, Blob, Principal, State, Tree, TreeEntry},
    store::ObjectStore,
};

use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    spool: uuid::Uuid,
) {
    let selected = Blob::from_slice(b"private source\n");
    let unrelated = Blob::from_slice(b"unrelated secret\n");
    repository.store().put_blob(&selected).expect("source");
    repository
        .store()
        .put_blob(&unrelated)
        .expect("unrelated object");
    let mut tree = Tree::new();
    tree.insert(TreeEntry::file("source.txt", selected.hash(), false).expect("entry"));
    repository.store().put_tree(&tree).expect("tree");
    let state = State::new_snapshot(
        tree.hash(),
        vec![],
        Attribution::human(Principal::new("Owner", "owner@test")),
    )
    .with_intent("Read exact source");
    repository.store().put_state(&state).expect("state");
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
    let request = ReadContentRequest {
        revision: Some(revision.clone()),
        selections: vec![
            ContentRead {
                selection_id: "source".into(),
                selection: Some(content_read::Selection::Blob(BlobRead {
                    source: Some(blob_read::Source::Path("source.txt".into())),
                    offset: 2,
                    length: 5,
                })),
            },
            ContentRead {
                selection_id: "summary".into(),
                selection: Some(content_read::Selection::State(StateRead {
                    include_summary: true,
                    ..Default::default()
                })),
            },
            ContentRead {
                selection_id: "tree".into(),
                selection: Some(content_read::Selection::Tree(TreeRead::default())),
            },
        ],
        ..Default::default()
    };
    let mut stream = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&request)
        .await
        .expect("content stream");
    let mut completions = 0;
    let mut blob = false;
    let mut summary = false;
    let mut entry = false;
    while let Some(event) = stream.next().await.expect("content response") {
        assert_eq!(event.revision.as_ref(), Some(&revision));
        match event.payload.expect("payload") {
            content_event::Payload::Blob(chunk) => {
                assert_eq!(chunk.data, b"ivate");
                assert_eq!(chunk.object_hash, selected.hash().as_bytes());
                assert!(chunk.range_complete);
                blob = true;
            }
            content_event::Payload::State(value) => {
                assert_eq!(value.intent, "Read exact source");
                summary = true;
            }
            content_event::Payload::TreeEntry(value) => {
                assert_eq!(value.path, "source.txt");
                entry = true;
            }
            content_event::Payload::SelectionComplete(status) => {
                assert_eq!(status.coverage, Coverage::Complete as i32);
                completions += 1;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(blob && summary && entry);
    assert_eq!(completions, 3);
    for source in [
        blob_read::Source::ObjectHash(unrelated.hash().as_bytes().to_vec()),
        blob_read::Source::Path("../source.txt".into()),
    ] {
        let denied = ReadContentRequest {
            revision: Some(revision.clone()),
            selections: vec![ContentRead {
                selection_id: "denied".into(),
                selection: Some(content_read::Selection::Blob(BlobRead {
                    source: Some(source),
                    ..Default::default()
                })),
            }],
            ..Default::default()
        };
        let mut stream = remote
            .api
            .observe::<thread_api::rpc::ContentServiceReadContent>(&denied)
            .await
            .expect("denied stream");
        assert!(
            stream.next().await.is_err(),
            "unreachable object and invalid path require typed failure"
        );
    }
    let blobs = remote
        .read_blobs(
            revision,
            vec![thread_api::content::BlobSource::ObjectHash(
                selected.hash().as_bytes().to_vec(),
            )],
        )
        .await
        .expect("SDK exact blob reader");
    assert_eq!(blobs[0].bytes, b"private source\n");
}
