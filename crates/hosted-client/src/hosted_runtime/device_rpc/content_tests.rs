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
    device: &DeviceRpc,
    spool: uuid::Uuid,
) {
    let selected = Blob::from_slice(b"private source\n");
    let unrelated = Blob::from_slice(b"unrelated secret\n");
    repository.store().put_blob(&selected).expect("source");
    repository
        .store()
        .put_blob(&unrelated)
        .expect("unrelated object");
    let hidden = Blob::from_slice(b"hidden signed entry\n");
    repository.store().put_blob(&hidden).expect("hidden object");
    let marked = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::file("visible.txt", selected.hash(), false).expect("visible entry"),
            TreeEntry::file("hidden.txt", hidden.hash(), false).expect("hidden entry"),
        ],
        vec![[1; 32], [2; 32]],
    )
    .expect("salted source tree");
    repository.store().put_tree(&marked).expect("salted tree");
    let hidden_index = marked
        .entries()
        .iter()
        .position(|entry| entry.name() == "hidden.txt")
        .expect("hidden index");
    let mut redactions = objects::object::EntryRedactions::default();
    redactions.extend_overrides(
        &[objects::object::EntryVisibilityEntry {
            tree_id: marked.hash(),
            leaf_hash: marked.v4_leaf_hash_at(hidden_index).expect("salted leaf"),
            tier: objects::object::VisibilityTier::Private {
                scope_label: "security".into(),
            },
        }],
        |tier| objects::object::visible(tier, &objects::object::AudienceTier::Internal),
    );
    let mut work = 0;
    assert!(
        super::content::visible_path_entry(
            repository.store(),
            marked.hash(),
            "hidden.txt",
            &redactions,
            &mut work
        )
        .is_err()
    );
    assert!(
        super::content::blob_hash(
            repository.store(),
            &State::new_snapshot(
                marked.hash(),
                vec![],
                Attribution::human(Principal::new("Owner", "owner@test"))
            ),
            &redactions,
            &BlobRead {
                source: Some(blob_read::Source::ObjectHash(
                    hidden.hash().as_bytes().to_vec()
                )),
                ..Default::default()
            },
            &mut work
        )
        .is_err()
    );
    assert!(
        super::content::visible_path_entry(
            repository.store(),
            marked.hash(),
            "visible.txt",
            &redactions,
            &mut work
        )
        .is_ok()
    );
    let projected = super::content_detail::project_visible_tree(
        repository,
        marked.hash(),
        &redactions,
        &mut work,
    )
    .expect("visible source projection");
    assert!(projected.get("hidden.txt").is_none());
    assert!(projected.get("visible.txt").is_some());
    let changed_visible = Blob::from_slice(b"visible change\n");
    repository
        .store()
        .put_blob(&changed_visible)
        .expect("changed visible blob");
    let changed = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::file("visible.txt", changed_visible.hash(), false).expect("changed entry"),
            TreeEntry::file("hidden.txt", hidden.hash(), false).expect("unchanged hidden entry"),
        ],
        vec![[3; 32], [2; 32]],
    )
    .expect("changed salted source tree");
    repository.store().put_tree(&changed).expect("changed tree");
    let projected_changed = super::content_detail::project_visible_tree(
        repository,
        changed.hash(),
        &redactions,
        &mut work,
    )
    .expect("changed visible projection");
    let report = verbs::diff::compute_projected_tree_diff(
        repository,
        &projected,
        &projected_changed,
        "base",
        "head",
        3,
    )
    .expect("visible-only diff");
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.path == "visible.txt")
    );
    assert!(
        !report
            .changes
            .iter()
            .any(|change| change.path == "hidden.txt")
    );
    let tree = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("source.txt", selected.hash(), false).expect("entry")],
        vec![[41; 32]],
    )
    .expect("salted content tree");
    repository.store().put_tree(&tree).expect("tree");
    let state = State::new_snapshot(
        tree.hash(),
        vec![repository.head().expect("head").expect("source base")],
        Attribution::human(Principal::new("Owner", "owner@test")),
    )
    .with_intent("Read exact source");
    repository.store().put_state(&state).expect("state");
    let revision = RevisionRef {
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::common::StateId {
                value: state.id().as_bytes().to_vec(),
            },
        )),
    };
    let mut request = ReadContentRequest {
        thread: Some(ThreadRef {
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            id: Some(ThreadId {
                value: vec![99; 32],
            }),
        }),
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
                selection: Some(content_read::Selection::State(StateRead::default())),
            },
            ContentRead {
                selection_id: "tree".into(),
                selection: Some(content_read::Selection::Tree(TreeRead::default())),
            },
        ],
        ..Default::default()
    };
    let mut unadmitted = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&request)
        .await
        .expect("exact source request");
    assert!(
        matches!(
            unadmitted.next().await,
            Err(api::v2::client::ClientError::Transport(
                thread_api::transport::Error::Remote(_)
            ))
        ),
        "an object hash without an audience-authorized Thread cannot disclose source"
    );
    let replica = repository
        .create_native_thread(
            "content-source",
            repository.head().expect("head").expect("source base"),
            None,
            "owned source read fixture",
        )
        .expect("admitted local-key source Thread");
    repository
        .record_native_capture("content-source", state.id())
        .expect("original accepted source capture");
    let thread = ThreadRef {
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    request.thread = Some(thread.clone());
    let exact_search = SearchRequest {
        threads: vec![thread.clone()],
        domains: vec![SearchDomain::Revision as i32],
        text: format!("heddle:{}", state.id().to_string_full()),
        mode: search_request::Mode::Lexical as i32,
        ..Default::default()
    };
    let mut found = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&exact_search)
        .await
        .expect("indexed exact revision search");
    let mut hits = 0;
    while let Some(event) = found.next().await.expect("revision search frame") {
        if let Some(search_event::Payload::Hit(hit)) = event.payload {
            assert_eq!(hit.domain, SearchDomain::Revision as i32);
            assert_eq!(hit.match_kind, SearchMatchKind::HashExact as i32);
            assert_eq!(hit.thread.as_ref(), Some(&thread));
            assert!(matches!(hit.subject.and_then(|subject| subject.entity),
                Some(entity_ref::Entity::Revision(reference)) if reference == revision));
            assert!(
                matches!(hit.location.and_then(|location| location.revision), Some(reference) if reference == revision)
            );
            hits += 1;
        }
    }
    assert_eq!(
        hits, 1,
        "one accepted source operation in exact selected Thread"
    );
    let mut wrong_search = exact_search.clone();
    wrong_search.threads[0].id = Some(ThreadId {
        value: vec![75; 32],
    });
    let mut wrong = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&wrong_search)
        .await
        .expect("unrelated Thread search");
    while let Some(event) = wrong.next().await.expect("unrelated search frame") {
        assert!(
            !matches!(event.payload, Some(search_event::Payload::Hit(_))),
            "same source hash cannot escape through another Thread selector"
        );
    }
    let mut stream = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&request)
        .await
        .expect("content stream");
    device
        .content_rechecks
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let mut completions = 0;
    let mut frames = 0;
    let mut blob = false;
    let mut summary = false;
    let mut entry = false;
    while let Some(event) = stream.next().await.expect("content response") {
        frames += 1;
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
    assert!(frames > 3);
    assert!(
        device
            .content_rechecks
            .load(std::sync::atomic::Ordering::Relaxed)
            < frames as u64,
        "unchanged frames must not repeatedly scan source authority"
    );
    for source in [
        blob_read::Source::ObjectHash(unrelated.hash().as_bytes().to_vec()),
        blob_read::Source::Path("../source.txt".into()),
    ] {
        let denied = ReadContentRequest {
            thread: Some(thread.clone()),
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
            thread,
            revision,
            vec![thread_api::content::BlobSource::ObjectHash(
                selected.hash().as_bytes().to_vec(),
            )],
        )
        .await
        .expect("SDK exact blob reader");
    assert_eq!(blobs[0].bytes, b"private source\n");

    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    *device.content_send_gate.lock().expect("test gate") = Some(gate.clone());
    let late = ReadContentRequest {
        thread: Some(request.thread.expect("admitted thread")),
        revision: Some(request.revision.expect("admitted revision")),
        selections: vec![
            ContentRead {
                selection_id: "first".into(),
                selection: Some(content_read::Selection::State(StateRead::default())),
            },
            ContentRead {
                selection_id: "late".into(),
                selection: Some(content_read::Selection::Blob(BlobRead {
                    source: Some(blob_read::Source::Path("source.txt".into())),
                    ..Default::default()
                })),
            },
        ],
        ..Default::default()
    };
    let mut stream = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&late)
        .await
        .expect("late read");
    assert!(matches!(
        stream
            .next()
            .await
            .expect("first frame")
            .expect("first event")
            .payload,
        Some(content_event::Payload::State(_))
    ));
    let feed = device
        .feeds
        .lock()
        .expect("feeds")
        .get(&spool)
        .and_then(std::sync::Weak::upgrade)
        .expect("active feed");
    let mut changes = feed.changes.subscribe();
    let override_bytes = objects::object::EntryVisibility::new(
        state.change_id,
        tree.hash(),
        vec![objects::object::EntryVisibilityEntry {
            tree_id: tree.hash(),
            leaf_hash: tree.v4_leaf_hash_at(0).expect("source leaf"),
            tier: objects::object::VisibilityTier::Private {
                scope_label: "late-content".into(),
            },
        }],
    )
    .expect("late visibility descriptor")
    .encode()
    .expect("late visibility bytes");
    repository
        .restore_entry_visibility_sidecar(&state.change_id, Some(override_bytes))
        .expect("committed late visibility override");
    tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
        .await
        .expect("feed notification deadline")
        .expect("feed notification");
    gate.add_permits(1);
    let mut disclosed = false;
    let mut failed = false;
    loop {
        match stream.next().await {
            Ok(Some(event)) => {
                disclosed |= matches!(event.payload, Some(content_event::Payload::Blob(_)));
            }
            Ok(None) => break,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    *device.content_send_gate.lock().expect("test gate") = None;
    assert!(failed, "late override must fail the buffered read");
    assert!(!disclosed, "late override must withhold queued blob bytes");

    repository
        .put_state_visibility(objects::object::StateVisibility {
            state: state.id(),
            tier: objects::object::VisibilityTier::Private {
                scope_label: "search-hidden".into(),
            },
            embargo_until: None,
            declarer: Principal::new("Owner", "owner@test"),
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("local private State declaration");
    let mut withheld = remote
        .api
        .observe::<thread_api::rpc::SearchServiceSearch>(&exact_search)
        .await
        .expect("withheld exact revision search");
    let mut complete = false;
    while let Some(event) = withheld.next().await.expect("withheld search frame") {
        match event.payload {
            Some(search_event::Payload::Hit(_)) => {
                panic!("withheld source revision leaked through exact hash search")
            }
            Some(search_event::Payload::Complete(status)) => {
                assert_eq!(status.coverage, Coverage::Complete as i32);
                let page = status.page.expect("withheld page");
                assert!(
                    page.exhausted && page.next_page.is_empty(),
                    "hidden-only page must not expose a continuation"
                );
                complete = true;
            }
            _ => {}
        }
    }
    assert!(complete);
}
