// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{
    CallFailureCode, Discussion as ProtoDiscussion, DiscussionKind, DiscussionTurn as ProtoTurn,
    PathSymbolRef, RepoEvent, RepoEventKind, StateId as ProtoStateId, discussion_resolution,
};
use objects::object::{Attribution, CollaborationAnchor, Principal};
use repo::{CollaborationStore, Repository, migrate_legacy_discussions_once};
use tempfile::TempDir;

use super::{
    DiscussionCursorScope, DiscussionEventConsumer, DiscussionEventCursor, DiscussionEventOutcome,
    audience_cursor_scope, bootstrap_discussions, consume_discussion_event,
    consume_discussion_event_scoped, is_discussion_event, load_cursor, load_scoped_cursor,
    paired_thread_scope, parse_event_payload, save_cursor, save_scoped_cursor, subscribe_request,
};
use crate::hosted_runtime::hosted::test_server::CollaborationFixture;
use crate::client::HostedClient;

fn seed_repo() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
    repo.snapshot_with_attribution(
        Some("seed".to_string()),
        None,
        Attribution::human(Principal::new("Test", "test@example.com")),
    )
    .unwrap();
    (temp, repo)
}

fn load_event_cursor(
    repo: &Repository,
    client: &HostedClient,
    repo_path: &str,
) -> DiscussionEventCursor {
    load_scoped_cursor(repo.heddle_dir(), &audience_cursor_scope(client, repo_path)).unwrap()
}

fn opened_event(event_id: i64, discussion_id: &str, body: &str, turn_id: &str) -> RepoEvent {
    RepoEvent {
        event_id,
        repo_id: "repo-1".to_string(),
        event_type: "discussion.opened".to_string(),
        payload_json: serde_json::json!({
            "discussion_id": discussion_id,
            "file": "lib.rs",
            "symbol": "run",
            "visibility": "internal",
            "body": body,
            "author_name": "Ada",
            "author_email": "ada@example.com",
            "posted_at": 1_700_000_000,
            "turn_id": turn_id,
            "turn_seq": 1,
        })
        .to_string(),
        // Self-contained opens require the event state. Tests that omit it
        // (doorbell-fetch) clear `new_state`.
        new_state: Some(ProtoStateId {
            value: vec![0x11; 32],
        }),
        ..RepoEvent::default()
    }
}

fn appended_event(event_id: i64, discussion_id: &str, body: &str, turn_id: &str, seq: u64) -> RepoEvent {
    RepoEvent {
        event_id,
        repo_id: "repo-1".to_string(),
        event_type: "turn.appended".to_string(),
        kind: RepoEventKind::DiscussionTurn as i32,
        payload_json: serde_json::json!({
            "discussion_id": discussion_id,
            "file": "lib.rs",
            "symbol": "run",
            "body": body,
            "author_name": "Ada",
            "author_email": "ada@example.com",
            "posted_at": 1_700_000_010,
            "turn_id": turn_id,
            "turn_seq": seq,
        })
        .to_string(),
        ..RepoEvent::default()
    }
}

fn resolved_event(event_id: i64, discussion_id: &str) -> RepoEvent {
    RepoEvent {
        event_id,
        repo_id: "repo-1".to_string(),
        event_type: "discussion.resolved".to_string(),
        payload_json: serde_json::json!({
            "discussion_id": discussion_id,
            "file": "lib.rs",
            "symbol": "run",
            "body": "first",
            "author_name": "Ada",
            "author_email": "ada@example.com",
            "posted_at": 1_700_000_000,
            "turn_id": "turn-1",
            "turn_seq": 1,
            "resolution": { "kind": "dismissed", "reason": "done" },
        })
        .to_string(),
        ..RepoEvent::default()
    }
}

#[test]
fn discussion_event_types_are_recognized_and_others_are_not() {
    assert!(is_discussion_event(&RepoEvent {
        event_type: "discussion.opened".into(),
        ..RepoEvent::default()
    }));
    assert!(is_discussion_event(&RepoEvent {
        event_type: "turn.appended".into(),
        ..RepoEvent::default()
    }));
    assert!(is_discussion_event(&RepoEvent {
        event_type: "discussion.resolved".into(),
        ..RepoEvent::default()
    }));
    assert!(is_discussion_event(&RepoEvent {
        event_type: "something.else".into(),
        kind: RepoEventKind::DiscussionTurn as i32,
        ..RepoEvent::default()
    }));
    assert!(!is_discussion_event(&RepoEvent {
        event_type: "ref.updated".into(),
        ..RepoEvent::default()
    }));
}

#[test]
fn payload_parser_pins_the_weft_flat_doorbell_shape() {
    let event = RepoEvent {
        event_type: "turn.appended".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "turn_id": "turn-9",
            "turn_seq": 2,
            "body": "second",
            "author_name": "bob",
            "author_email": "bob@example.com",
            "posted_at": 9,
            "file": "a.rs",
            "symbol": "f"
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert_eq!(payload.discussion_id.as_deref(), Some("disc-9"));
    assert_eq!(payload.turn_id.as_deref(), Some("turn-9"));
    assert_eq!(payload.turn_seq, 2);
    assert_eq!(payload.body.as_deref(), Some("second"));

    let nested = RepoEvent {
        event_type: "turn.appended".into(),
        payload_json: serde_json::json!({
            "discussion": { "id": "disc-9", "file": "a.rs" },
            "turn": { "id": "turn-9", "turn_seq": 2, "body": "second" }
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let ignored = parse_event_payload(&nested);
    assert_eq!(ignored.discussion_id, None);
    assert_eq!(ignored.turn_id, None);
    assert_eq!(ignored.body, None);
}

#[test]
fn subscribe_request_filters_to_discussion_event_types() {
    let request = subscribe_request("repo-1", 7, "main", "thread-main");
    assert_eq!(request.repo_id, "repo-1");
    assert_eq!(request.after_event_id, 7);
    assert_eq!(
        request.event_types,
        ["discussion.opened", "turn.appended", "discussion.resolved"]
    );
    assert_eq!(request.thread, "main");
    assert_eq!(request.thread_id, "thread-main");
}

#[test]
fn paired_thread_scope_requires_both_name_and_stable_id() {
    assert_eq!(
        paired_thread_scope("", "").unwrap(),
        (String::new(), String::new())
    );
    assert_eq!(
        paired_thread_scope("foo", "thr-1").unwrap(),
        ("foo".to_string(), "thr-1".to_string())
    );
    assert!(paired_thread_scope("foo", "").is_err());
    assert!(paired_thread_scope("", "thr-1").is_err());
}

#[test]
fn cursor_round_trips_per_repo() {
    let temp = TempDir::new().unwrap();
    let cursor = DiscussionEventCursor {
        after_event_id: 41,
        repo_id: "repo-1".into(),
        bootstrapped: true,
    };
    save_cursor(temp.path(), "acme/widgets", &cursor).unwrap();
    assert_eq!(load_cursor(temp.path(), "acme/widgets").unwrap(), cursor);
    assert_eq!(
        load_cursor(temp.path(), "other/repo").unwrap(),
        DiscussionEventCursor::default()
    );
}

#[tokio::test]
async fn opened_and_appended_events_materialize_distinct_turns_and_advance_watermark() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-live-1".to_string(),
        proto_discussion("disc-live-1", &[("turn-open", "keep this invariant", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let opened = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &opened_event(11, "disc-live-1", "keep this invariant", "turn-open"),
    )
    .await
    .unwrap();
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-live-1"],
        "opened must GetDiscussion even with a fat payload"
    );
    assert!(matches!(
        opened,
        DiscussionEventOutcome::Applied { discussion_id } if discussion_id == "disc-live-1"
    ));

    let appended = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &appended_event(12, "disc-live-1", "second turn", "turn-append", 2),
    )
    .await
    .unwrap();
    assert!(appended.applied());

    let replay = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &appended_event(12, "disc-live-1", "second turn", "turn-append", 2),
    )
    .await
    .unwrap();
    assert!(matches!(
        replay,
        DiscussionEventOutcome::Unchanged { discussion_id } if discussion_id == "disc-live-1"
    ));

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let materialized = store.materialize().unwrap();
    assert_eq!(materialized.discussions.len(), 1);
    let discussion = materialized.discussions.values().next().unwrap();
    assert_eq!(discussion.turns.len(), 2);
    assert_eq!(discussion.turns[0].1.body, "keep this invariant");
    assert_eq!(discussion.turns[1].1.body, "second turn");

    let cursor = load_event_cursor(&repo, &client, "acme/widgets");
    assert_eq!(cursor.after_event_id, 12);
    assert_eq!(cursor.repo_id, "repo-1");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn resolved_event_writes_a_local_resolution() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-res".to_string(),
        proto_dismissed_discussion(
            "disc-res",
            &[("turn-1", "first", 1)],
            "done",
        ),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;
    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &resolved_event(2, "disc-res"),
    )
    .await
    .unwrap();
    assert!(outcome.applied());
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-res"]
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store.materialize().unwrap().discussions.into_values().next().unwrap();
    assert!(discussion.resolution.is_some());

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn missing_discussion_id_is_skipped_and_still_advances_the_watermark() {
    let (_temp, repo) = seed_repo();
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
    let event = RepoEvent {
        event_id: 99,
        event_type: "turn.appended".into(),
        payload_json: r#"{"body":"orphan"}"#.into(),
        ..RepoEvent::default()
    };
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &event)
        .await
        .unwrap();
    assert!(matches!(outcome, DiscussionEventOutcome::Skipped { .. }));
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        99
    );
    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn unknown_event_types_are_ignored_without_touching_the_op_log() {
    let (_temp, repo) = seed_repo();
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
    let event = RepoEvent {
        event_id: 5,
        event_type: "ref.updated".into(),
        payload_json: "{}".into(),
        new_state: Some(ProtoStateId {
            value: [1u8; 32].to_vec(),
        }),
        ..RepoEvent::default()
    };
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &event)
        .await
        .unwrap();
    assert_eq!(outcome, DiscussionEventOutcome::Ignored);
    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    assert!(store.materialize().unwrap().discussions.is_empty());
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        5
    );
    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn bootstrap_marks_the_cursor_then_live_events_append() {
    let (_temp, repo) = seed_repo();
    let head = repo.head().unwrap().unwrap();
    let bootstrap = vec![objects::object::Discussion {
        id: "server-boot".to_string(),
        anchor: objects::object::SymbolAnchor::new("lib.rs", "run"),
        opened_against_state: head,
        opened_at: 1_700_000_000,
        thread_ref: None,
        turns: vec![objects::object::DiscussionTurn {
            author: Principal::new("Reviewer", "reviewer@example.com"),
            body: "from snapshot".to_string(),
            posted_at: 1_700_000_001,
            references: Vec::new(),
        }],
        resolution: objects::object::DiscussionResolution::Open,
        body_changed_since_open: false,
        anchor_ambiguous: false,
        orphaned: false,
        visibility: objects::object::VisibilityTier::Internal,
        resolved_annotation_id: None,
    }];
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
    let cursor = bootstrap_discussions(&repo, &mut client, "acme/widgets", Some(&bootstrap))
        .await
        .unwrap();
    assert!(cursor.bootstrapped);

    consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &appended_event(20, "server-boot", "live turn", "turn-live", 2),
    )
    .await
    .unwrap();

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store.materialize().unwrap().discussions.into_values().next().unwrap();
    assert_eq!(discussion.turns.len(), 2);
    assert_eq!(discussion.turns[1].1.body, "live turn");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn replaying_opened_after_bootstrap_does_not_duplicate_the_first_turn() {
    let (_temp, repo) = seed_repo();
    let head = repo.head().unwrap().unwrap();
    let bootstrap = vec![objects::object::Discussion {
        id: "server-boot".to_string(),
        anchor: objects::object::SymbolAnchor::new("lib.rs", "run"),
        opened_against_state: head,
        opened_at: 1_700_000_000,
        thread_ref: None,
        turns: vec![objects::object::DiscussionTurn {
            author: Principal::new("Ada", "ada@example.com"),
            body: "keep this invariant".to_string(),
            posted_at: 1_700_000_000,
            references: Vec::new(),
        }],
        resolution: objects::object::DiscussionResolution::Open,
        body_changed_since_open: false,
        anchor_ambiguous: false,
        orphaned: false,
        visibility: objects::object::VisibilityTier::Internal,
        resolved_annotation_id: None,
    }];
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "server-boot".to_string(),
        proto_discussion("server-boot", &[("turn-open", "keep this invariant", 1)]),
    );
    let (mut client, server, _fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;
    bootstrap_discussions(&repo, &mut client, "acme/widgets", Some(&bootstrap))
        .await
        .unwrap();

    let replay = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &opened_event(3, "server-boot", "keep this invariant", "turn-open"),
    )
    .await
    .unwrap();
    assert!(matches!(
        replay,
        DiscussionEventOutcome::Unchanged { discussion_id } if discussion_id == "server-boot"
    ));

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store.materialize().unwrap().discussions.into_values().next().unwrap();
    assert_eq!(discussion.turns.len(), 1);
    assert_eq!(discussion.turns[0].1.body, "keep this invariant");

    client.close().await;
    server.await.unwrap();
}

#[test]
fn appended_without_turn_identity_is_a_doorbell() {
    let event = RepoEvent {
        event_type: "turn.appended".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "body": "second"
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert!(super::append_from_payload(&payload, true).is_none());
    assert!(super::append_from_payload(&payload, false).is_none());
}

fn doorbell(
    event_id: i64,
    event_type: &str,
    discussion_id: &str,
    turn_id: &str,
    turn_seq: u64,
) -> RepoEvent {
    RepoEvent {
        event_id,
        repo_id: "00000000-0000-0000-0000-000000000001".to_string(),
        event_type: event_type.to_string(),
        kind: if event_type == "turn.appended" {
            RepoEventKind::DiscussionTurn as i32
        } else {
            0
        },
        payload_json: serde_json::json!({
            "discussion_id": discussion_id,
            "turn_id": turn_id,
            "turn_seq": turn_seq,
        })
        .to_string(),
        ..RepoEvent::default()
    }
}

fn proto_discussion(id: &str, turns: &[(&str, &str, u64)]) -> ProtoDiscussion {
    ProtoDiscussion {
        id: id.to_string(),
        anchor: Some(PathSymbolRef {
            file: "lib.rs".to_string(),
            symbol: "run".to_string(),
        }),
        visibility: "internal".to_string(),
        turns: turns
            .iter()
            .map(|(turn_id, body, seq)| ProtoTurn {
                author_name: "Ada".to_string(),
                author_email: "ada@example.com".to_string(),
                body: (*body).to_string(),
                turn_id: (*turn_id).to_string(),
                turn_seq: *seq,
                posted_at: Some(prost_types::Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                }),
                ..ProtoTurn::default()
            })
            .collect(),
        ..ProtoDiscussion::default()
    }
}

fn proto_dismissed_discussion(
    id: &str,
    turns: &[(&str, &str, u64)],
    reason: &str,
) -> ProtoDiscussion {
    let mut discussion = proto_discussion(id, turns);
    discussion.resolution = Some(api::heddle::api::v1alpha1::DiscussionResolution {
        state: Some(discussion_resolution::State::Dismissed(
            discussion_resolution::Dismissed {
                reason: reason.to_string(),
            },
        )),
    });
    discussion
}

#[tokio::test]
async fn doorbell_payload_fetches_get_discussion_and_materializes() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-bell".to_string(),
        proto_discussion("disc-bell", &[("turn-open", "keep this invariant", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(11, "discussion.opened", "disc-bell", "turn-open", 1),
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        DiscussionEventOutcome::Applied { discussion_id } if discussion_id == "disc-bell"
    ));
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-bell"]
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .unwrap();
    assert_eq!(discussion.turns.len(), 1);
    assert_eq!(discussion.turns[0].1.body, "keep this invariant");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn get_discussion_permission_denied_is_skipped_and_advances_the_watermark() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture
        .hidden
        .insert("disc-hidden".to_string(), CallFailureCode::PermissionDenied);
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(44, "turn.appended", "disc-hidden", "turn-2", 2),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DiscussionEventOutcome::Skipped { .. }));
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        44
    );
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-hidden"]
    );
    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    assert!(store.materialize().unwrap().discussions.is_empty());

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn get_discussion_not_found_is_skipped_and_advances_the_watermark() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture
        .hidden
        .insert("disc-gone".to_string(), CallFailureCode::NotFound);
    let (mut client, server, _fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(45, "discussion.opened", "disc-gone", "turn-1", 1),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DiscussionEventOutcome::Skipped { .. }));
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        45
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn bootstrap_none_hits_list_by_state() {
    let (_temp, repo) = seed_repo();
    let fixture = CollaborationFixture {
        list: vec![proto_discussion(
            "disc-list",
            &[("turn-open", "from list", 1)],
        )],
        ..CollaborationFixture::default()
    };
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let cursor = bootstrap_discussions(&repo, &mut client, "acme/widgets", None)
        .await
        .unwrap();
    assert!(cursor.bootstrapped);
    assert_eq!(
        *fixture
            .list_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        1
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .unwrap();
    assert_eq!(discussion.turns[0].1.body, "from list");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn fat_append_without_mirror_fetches_instead_of_opening_at_turn_two() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-mid".to_string(),
        proto_discussion(
            "disc-mid",
            &[
                ("turn-open", "first turn", 1),
                ("turn-append", "second turn", 2),
            ],
        ),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &appended_event(12, "disc-mid", "second turn", "turn-append", 2),
    )
    .await
    .unwrap();
    assert!(outcome.applied());
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-mid"]
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .unwrap();
    assert_eq!(discussion.turns.len(), 2);
    assert_eq!(discussion.turns[0].1.body, "first turn");
    assert_eq!(discussion.turns[1].1.body, "second turn");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn consume_next_resumes_after_the_stream_ends() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture {
        one_event_per_subscribe: true,
        events: vec![
            doorbell(1, "discussion.opened", "disc-live", "turn-open", 1),
            doorbell(2, "turn.appended", "disc-live", "turn-append", 2),
        ],
        ..CollaborationFixture::default()
    };
    fixture.discussions.insert(
        "disc-live".to_string(),
        proto_discussion(
            "disc-live",
            &[
                ("turn-open", "first turn", 1),
                ("turn-append", "second turn", 2),
            ],
        ),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let mut consumer = DiscussionEventConsumer::new(&repo, &mut client, "acme/widgets");
    let mut subscription = consumer.start(None).await.unwrap();

    let (first, _) = consumer.consume_next(&mut subscription).await.unwrap();
    assert_eq!(first.event_id, 1);
    let (second, _) = consumer.consume_next(&mut subscription).await.unwrap();
    assert_eq!(second.event_id, 2);
    assert_eq!(
        fixture
            .subscribe_after
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        [0, 1]
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .unwrap();
    assert_eq!(discussion.turns.len(), 2);

    client.close().await;
    server.await.unwrap();
}

#[test]
fn opened_without_visibility_is_a_doorbell() {
    let event = RepoEvent {
        event_type: "discussion.opened".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "body": "hello",
            "file": "lib.rs",
            "symbol": "run",
            "turn_id": "turn-1",
            "turn_seq": 1
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert!(super::append_from_payload(&payload, false).is_none());
    assert!(super::append_from_payload(&payload, true).is_none());
}

#[test]
fn fat_append_on_unknown_discussion_is_not_self_contained() {
    let event = appended_event(2, "disc-unknown", "second turn", "turn-2", 2);
    let payload = parse_event_payload(&event);
    assert!(super::append_from_payload(&payload, false).is_none());
    assert!(super::append_from_payload(&payload, true).is_some());
}

#[test]
fn fat_append_with_turn_id_but_zero_seq_is_a_doorbell() {
    let event = RepoEvent {
        event_type: "turn.appended".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "file": "lib.rs",
            "symbol": "run",
            "body": "second turn",
            "author_name": "Ada",
            "author_email": "ada@example.com",
            "posted_at": 1_700_000_010,
            "turn_id": "turn-append",
            "turn_seq": 0
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert!(super::append_from_payload(&payload, true).is_none());
    assert!(super::append_from_payload(&payload, false).is_none());
}

#[test]
fn opened_without_state_id_is_a_doorbell() {
    let event = RepoEvent {
        event_type: "discussion.opened".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "file": "lib.rs",
            "symbol": "run",
            "visibility": "internal",
            "body": "hello",
            "turn_id": "turn-1",
            "turn_seq": 1
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert!(event.new_state.is_none());
    assert!(payload.opened_against_state.is_none());
    assert!(super::append_from_payload(&payload, false).is_none());
    assert!(super::append_from_payload(&payload, true).is_none());
}

#[test]
fn principals_do_not_share_a_cursor_slot() {
    let temp = TempDir::new().unwrap();
    let alice = DiscussionCursorScope {
        repo_path: "acme/widgets".into(),
        principal: "alice".into(),
        ..DiscussionCursorScope::default()
    };
    let bob = DiscussionCursorScope {
        repo_path: "acme/widgets".into(),
        principal: "bob".into(),
        ..DiscussionCursorScope::default()
    };
    let cursor = DiscussionEventCursor {
        after_event_id: 101,
        repo_id: "repo-1".into(),
        bootstrapped: true,
    };
    save_scoped_cursor(temp.path(), &alice, &cursor).unwrap();
    assert_eq!(
        load_scoped_cursor(temp.path(), &bob).unwrap(),
        DiscussionEventCursor::default(),
        "a different hosted principal must not inherit another audience's cursor"
    );
    assert_eq!(
        load_cursor(temp.path(), "acme/widgets").unwrap(),
        DiscussionEventCursor::default(),
        "the unscoped repo_path slot must not inherit a principal-scoped watermark"
    );
    assert_eq!(load_scoped_cursor(temp.path(), &alice).unwrap(), cursor);
}

#[test]
fn opened_without_file_or_symbol_is_a_doorbell() {
    let event = RepoEvent {
        event_type: "discussion.opened".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "visibility": "internal",
            "body": "hello",
            "turn_id": "turn-1",
            "turn_seq": 1
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert!(super::append_from_payload(&payload, false).is_none());
    assert!(super::append_from_payload(&payload, true).is_none());
}

#[test]
fn resolved_without_a_real_resolution_is_a_doorbell() {
    let event = RepoEvent {
        event_type: "discussion.resolved".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "file": "lib.rs",
            "symbol": "run",
            "body": "first",
            "turn_id": "turn-1",
            "turn_seq": 1
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert!(super::append_from_payload(&payload, true).is_none());
    assert!(super::append_from_payload(&payload, false).is_none());
}

#[test]
fn filtered_cursor_slot_does_not_share_the_unfiltered_watermark() {
    let temp = TempDir::new().unwrap();
    let filtered = DiscussionCursorScope {
        repo_path: "acme/widgets".into(),
        thread: "foo".into(),
        ..DiscussionCursorScope::default()
    };
    let cursor = DiscussionEventCursor {
        after_event_id: 41,
        repo_id: "repo-1".into(),
        bootstrapped: true,
    };
    save_scoped_cursor(temp.path(), &filtered, &cursor).unwrap();
    assert_eq!(
        load_cursor(temp.path(), "acme/widgets").unwrap(),
        DiscussionEventCursor::default()
    );
    assert_eq!(
        load_scoped_cursor(temp.path(), &filtered).unwrap(),
        cursor
    );
}

#[test]
fn authority_scoped_cursor_does_not_inherit_the_legacy_repo_path_slot() {
    let temp = TempDir::new().unwrap();
    let legacy = DiscussionEventCursor {
        after_event_id: 7,
        repo_id: "repo-1".into(),
        bootstrapped: true,
    };
    save_cursor(temp.path(), "acme/widgets", &legacy).unwrap();
    let other_host = DiscussionCursorScope {
        authority: "other.example:443".into(),
        repo_path: "acme/widgets".into(),
        ..DiscussionCursorScope::default()
    };
    assert_eq!(
        load_scoped_cursor(temp.path(), &other_host).unwrap(),
        DiscussionEventCursor::default(),
        "a different hosted authority must not inherit the bare repo_path cursor"
    );
    assert_eq!(
        load_cursor(temp.path(), "acme/widgets").unwrap(),
        legacy
    );
}

#[tokio::test]
async fn filtered_wait_does_not_advance_the_unfiltered_watermark() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-foo".to_string(),
        proto_discussion("disc-foo", &[("turn-1", "from foo", 1)]),
    );
    let (mut client, server, _fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;
    let filtered = DiscussionCursorScope {
        repo_path: "acme/widgets".into(),
        thread: "foo".into(),
        ..DiscussionCursorScope::default()
    };
    let outcome = consume_discussion_event_scoped(
        &repo,
        &mut client,
        "acme/widgets",
        &filtered,
        &opened_event(10, "disc-foo", "from foo", "turn-1"),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DiscussionEventOutcome::Applied { .. }));
    assert_eq!(
        load_cursor(repo.heddle_dir(), "acme/widgets")
            .unwrap()
            .after_event_id,
        0
    );
    assert_eq!(
        load_scoped_cursor(repo.heddle_dir(), &filtered)
            .unwrap()
            .after_event_id,
        10
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn get_discussion_is_called_with_the_event_state_id() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-prior".to_string(),
        proto_discussion("disc-prior", &[("turn-open", "keep this invariant", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let mut event = doorbell(21, "discussion.opened", "disc-prior", "turn-open", 1);
    event.new_state = Some(ProtoStateId {
        value: vec![0xab; 32],
    });
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &event)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        DiscussionEventOutcome::Applied { discussion_id } if discussion_id == "disc-prior"
    ));
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-prior"]
    );
    assert_eq!(
        fixture
            .get_request_state_ids
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        [Some(vec![0xab; 32])]
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn incomplete_open_without_anchor_doorbell_fetches() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-open".to_string(),
        proto_discussion("disc-open", &[("turn-open", "keep this invariant", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let event = RepoEvent {
        event_id: 31,
        repo_id: "repo-1".to_string(),
        event_type: "discussion.opened".to_string(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-open",
            "visibility": "internal",
            "body": "hello without an anchor",
            "turn_id": "turn-open",
            "turn_seq": 1
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &event)
        .await
        .unwrap();
    assert!(matches!(outcome, DiscussionEventOutcome::Applied { .. }));
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-open"]
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn incomplete_resolve_without_resolution_doorbell_fetches() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-res".to_string(),
        proto_discussion("disc-res", &[("turn-1", "first", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &opened_event(1, "disc-res", "first", "turn-1"),
    )
    .await
    .unwrap();

    let event = RepoEvent {
        event_id: 32,
        repo_id: "repo-1".to_string(),
        event_type: "discussion.resolved".to_string(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-res",
            "file": "lib.rs",
            "symbol": "run",
            "body": "first",
            "turn_id": "turn-1",
            "turn_seq": 1
        })
        .to_string(),
        ..RepoEvent::default()
    };
    consume_discussion_event(&repo, &mut client, "acme/widgets", &event)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-res", "disc-res"],
        "opened and resolved are both doorbells"
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn fat_append_with_turn_id_and_zero_seq_fetches_and_keeps_the_new_turn() {
    let (_temp, repo) = seed_repo();
    let head = repo.head().unwrap().unwrap();
    let bootstrap = vec![objects::object::Discussion {
        id: "disc-zero".to_string(),
        anchor: objects::object::SymbolAnchor::new("lib.rs", "run"),
        opened_against_state: head,
        opened_at: 1_700_000_000,
        thread_ref: None,
        turns: vec![objects::object::DiscussionTurn {
            author: Principal::new("Ada", "ada@example.com"),
            body: "first turn".to_string(),
            posted_at: 1_700_000_000,
            references: Vec::new(),
        }],
        resolution: objects::object::DiscussionResolution::Open,
        body_changed_since_open: false,
        anchor_ambiguous: false,
        orphaned: false,
        visibility: objects::object::VisibilityTier::Internal,
        resolved_annotation_id: None,
    }];
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-zero".to_string(),
        proto_discussion(
            "disc-zero",
            &[
                ("turn-open", "first turn", 1),
                ("turn-append", "second turn", 2),
            ],
        ),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;
    bootstrap_discussions(&repo, &mut client, "acme/widgets", Some(&bootstrap))
        .await
        .unwrap();

    let fat_zero = RepoEvent {
        event_id: 2,
        repo_id: "repo-1".to_string(),
        event_type: "turn.appended".to_string(),
        kind: RepoEventKind::DiscussionTurn as i32,
        payload_json: serde_json::json!({
            "discussion_id": "disc-zero",
            "file": "lib.rs",
            "symbol": "run",
            "body": "second turn",
            "author_name": "Ada",
            "author_email": "ada@example.com",
            "posted_at": 1_700_000_010,
            "turn_id": "turn-append",
            "turn_seq": 0
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &fat_zero)
        .await
        .unwrap();
    assert!(
        outcome.applied(),
        "a new turn_id must be kept, not dropped as ordinal 0"
    );
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-zero"],
        "already-mirrored zero-seq append cannot use the fast path"
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .unwrap();
    assert_eq!(discussion.turns.len(), 2);
    assert_eq!(discussion.turns[0].1.body, "first turn");
    assert_eq!(discussion.turns[1].1.body, "second turn");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn thread_scoped_subscribe_request_carries_thread_id() {
    let (_temp, repo) = seed_repo();
    let fixture = CollaborationFixture {
        events: vec![opened_event(1, "disc-1", "hello", "turn-1")],
        ..CollaborationFixture::default()
    };
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let mut consumer = DiscussionEventConsumer::new(&repo, &mut client, "acme/widgets")
        .with_thread("feature/run", "thr-stable");
    let mut subscription = consumer.start(None).await.unwrap();
    // start() can return before the server records the request body.
    // consume_next waits for the first event, which is written only after
    // serve_subscribe_repo_events has stored the paired thread fields.
    let (event, _) = consumer.consume_next(&mut subscription).await.unwrap();
    assert_eq!(event.event_id, 1);
    assert_eq!(
        fixture
            .subscribe_thread
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        [("feature/run".to_string(), "thr-stable".to_string())]
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn get_discussion_unauthenticated_does_not_advance_the_watermark() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture
        .hidden
        .insert("disc-auth".to_string(), CallFailureCode::Unauthenticated);
    let (mut client, server, _fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let error = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(55, "discussion.opened", "disc-auth", "turn-1", 1),
    )
    .await
    .expect_err("unauthenticated doorbell fetch must be fatal");
    assert!(
        error.to_string().contains("disc-auth") || error.to_string().contains("Unauthenticated"),
        "expected a fetch failure, got {error:#}"
    );
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        0,
        "unauthenticated must not advance the watermark"
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn opened_without_state_id_fetches_get_discussion() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-nostate".to_string(),
        proto_discussion("disc-nostate", &[("turn-open", "keep this invariant", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let mut event = opened_event(61, "disc-nostate", "hello", "turn-open");
    event.new_state = None;
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &event)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        DiscussionEventOutcome::Applied { discussion_id } if discussion_id == "disc-nostate"
    ));
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-nostate"],
        "missing event state must doorbell-fetch instead of opening against local HEAD"
    );

    client.close().await;
    server.await.unwrap();
}

fn proto_coordination_discussion(id: &str, turns: &[(&str, &str, u64)]) -> ProtoDiscussion {
    ProtoDiscussion {
        id: id.to_string(),
        kind: DiscussionKind::Coordination as i32,
        thread_ref: "feature/run".to_string(),
        visibility: "internal".to_string(),
        anchor: None,
        turns: turns
            .iter()
            .map(|(turn_id, body, seq)| ProtoTurn {
                author_name: "Ada".to_string(),
                author_email: "ada@example.com".to_string(),
                body: (*body).to_string(),
                turn_id: (*turn_id).to_string(),
                turn_seq: *seq,
                posted_at: Some(prost_types::Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                }),
                ..ProtoTurn::default()
            })
            .collect(),
        ..ProtoDiscussion::default()
    }
}

#[tokio::test]
async fn coordination_doorbell_advances_watermark_and_does_not_fail_loop() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-coord".to_string(),
        proto_coordination_discussion("disc-coord", &[("turn-open", "handoff the review", 1)]),
    );
    fixture.discussions.insert(
        "disc-later".to_string(),
        proto_discussion("disc-later", &[("turn-1", "a later code discussion", 1)]),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(71, "discussion.opened", "disc-coord", "turn-open", 1),
    )
    .await
    .expect("coordination must not fail-loop the event stream");
    assert!(
        outcome.applied() || matches!(outcome, DiscussionEventOutcome::Skipped { .. }),
        "expected applied or skipped, got {outcome:?}"
    );
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        71,
        "an empty-anchor coordination doorbell must still advance the watermark"
    );
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-coord"]
    );

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .expect("coordination should materialize with a repository anchor");
    assert_eq!(discussion.anchor, CollaborationAnchor::Repository);

    let later = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &opened_event(72, "disc-later", "a later code discussion", "turn-1"),
    )
    .await
    .expect("later events must still deliver after a coordination doorbell");
    assert!(matches!(later, DiscussionEventOutcome::Applied { .. }));
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        72
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn opened_event_never_applies_without_get_discussion() {
    let (_temp, repo) = seed_repo();
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(
            CollaborationFixture::default(),
        )
        .await;

    let fat = opened_event(81, "disc-fat", "keep this invariant", "turn-open");
    assert!(
        fat.payload_json.contains("\"file\"")
            && fat.payload_json.contains("\"symbol\"")
            && fat.payload_json.contains("\"visibility\"")
            && fat.payload_json.contains("\"body\""),
        "this test requires a fat opened payload"
    );
    let outcome = consume_discussion_event(&repo, &mut client, "acme/widgets", &fat)
        .await
        .unwrap();
    assert!(
        !outcome.applied(),
        "opened is a doorbell; a fat payload must not reconstruct a HostedDiscussion: {outcome:?}"
    );
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-fat"],
        "opened must always GetDiscussion"
    );
    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    assert!(
        store.materialize().unwrap().discussions.is_empty(),
        "fat opened without a snapshot must not write the op-log"
    );
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        81,
        "NotFound skip still advances so the event does not fail-loop"
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn dismissed_empty_reason_fetches_or_skips_and_does_not_fail_loop() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-empty-dismiss".to_string(),
        proto_dismissed_discussion("disc-empty-dismiss", &[("turn-1", "first", 1)], ""),
    );
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &resolved_event(82, "disc-empty-dismiss"),
    )
    .await
    .expect("empty dismiss reason must not fail-loop");
    assert!(
        matches!(
            outcome,
            DiscussionEventOutcome::Skipped { .. } | DiscussionEventOutcome::Applied { .. }
        ),
        "expected fetch+skip or apply, got {outcome:?}"
    );
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-empty-dismiss"]
    );
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        82,
        "empty dismiss reason must advance the watermark"
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn discussion_bodies_keep_leading_and_trailing_whitespace() {
    let (_temp, repo) = seed_repo();
    let padded = "  keep padding  ";
    let mut fixture = CollaborationFixture::default();
    fixture.discussions.insert(
        "disc-pad".to_string(),
        proto_discussion("disc-pad", &[("turn-open", padded, 1)]),
    );
    let (mut client, server, _fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(83, "discussion.opened", "disc-pad", "turn-open", 1),
    )
    .await
    .unwrap();

    let appended = appended_event(84, "disc-pad", "  second pad  ", "turn-append", 2);
    consume_discussion_event(&repo, &mut client, "acme/widgets", &appended)
        .await
        .unwrap();

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .unwrap();
    assert_eq!(discussion.turns[0].1.body, padded);
    assert_eq!(discussion.turns[1].1.body, "  second pad  ");

    client.close().await;
    server.await.unwrap();
}

#[test]
fn payload_body_keeps_leading_and_trailing_whitespace() {
    let event = RepoEvent {
        event_type: "turn.appended".into(),
        payload_json: serde_json::json!({
            "discussion_id": "disc-9",
            "turn_id": "turn-9",
            "turn_seq": 2,
            "body": "  keep padding  ",
            "author_name": "Ada",
            "posted_at": 9
        })
        .to_string(),
        ..RepoEvent::default()
    };
    let payload = parse_event_payload(&event);
    assert_eq!(payload.body.as_deref(), Some("  keep padding  "));
}

#[tokio::test]
async fn empty_anchor_get_discussion_uses_repository_and_advances() {
    let (_temp, repo) = seed_repo();
    let mut proto = proto_discussion("disc-empty-anchor", &[("turn-open", "repo wide", 1)]);
    proto.anchor = Some(PathSymbolRef {
        file: String::new(),
        symbol: String::new(),
    });
    let mut fixture = CollaborationFixture::default();
    fixture
        .discussions
        .insert("disc-empty-anchor".to_string(), proto);
    let (mut client, server, fixture) =
        crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &doorbell(85, "discussion.opened", "disc-empty-anchor", "turn-open", 1),
    )
    .await
    .expect("empty-anchor GetDiscussion must not fail-loop");
    assert!(
        outcome.applied() || matches!(outcome, DiscussionEventOutcome::Skipped { .. }),
        "expected applied or skipped, got {outcome:?}"
    );
    assert_eq!(
        load_event_cursor(&repo, &client, "acme/widgets").after_event_id,
        85
    );
    assert_eq!(
        fixture
            .get_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_slice(),
        ["disc-empty-anchor"]
    );
    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion = store
        .materialize()
        .unwrap()
        .discussions
        .into_values()
        .next()
        .expect("empty-anchor snapshot should materialize as Repository");
    assert_eq!(discussion.anchor, CollaborationAnchor::Repository);

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn bootstrap_claims_legacy_migration_marker_before_local_import() {
    let (_temp, repo) = seed_repo();
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
    bootstrap_discussions(&repo, &mut client, "acme/widgets", Some(&[]))
        .await
        .unwrap();

    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let report = migrate_legacy_discussions_once(&repo, &store, repo.get_attribution().unwrap())
        .expect("legacy migration after hosted bootstrap");
    assert!(
        report.is_none(),
        "pull_discussions must claim the legacy marker so Wait cannot convert server-minted attachments first"
    );

    client.close().await;
    server.await.unwrap();
}
