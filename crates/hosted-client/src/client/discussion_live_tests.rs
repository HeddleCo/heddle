// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{
    CallFailureCode, Discussion as ProtoDiscussion, DiscussionTurn as ProtoTurn, PathSymbolRef,
    RepoEvent, RepoEventKind, StateId as ProtoStateId,
};
use objects::object::{Attribution, Principal};
use repo::{CollaborationStore, Repository};
use tempfile::TempDir;

use super::{
    DiscussionCursorScope, DiscussionEventConsumer, DiscussionEventCursor, DiscussionEventOutcome,
    bootstrap_discussions, consume_discussion_event, consume_discussion_event_scoped,
    is_discussion_event, load_cursor, load_scoped_cursor, parse_event_payload, save_cursor,
    save_scoped_cursor, subscribe_request,
};
use crate::hosted_runtime::hosted::HostedResolution;
use crate::hosted_runtime::hosted::test_server::CollaborationFixture;

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
    assert_eq!(payload.file.as_deref(), Some("a.rs"));

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
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;

    let opened = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &opened_event(11, "disc-live-1", "keep this invariant", "turn-open"),
    )
    .await
    .unwrap();
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

    let cursor = load_cursor(repo.heddle_dir(), "acme/widgets").unwrap();
    assert_eq!(cursor.after_event_id, 12);
    assert_eq!(cursor.repo_id, "repo-1");

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn resolved_event_writes_a_local_resolution() {
    let (_temp, repo) = seed_repo();
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
    consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &opened_event(1, "disc-res", "first", "turn-1"),
    )
    .await
    .unwrap();
    let outcome = consume_discussion_event(
        &repo,
        &mut client,
        "acme/widgets",
        &resolved_event(2, "disc-res"),
    )
    .await
    .unwrap();
    assert!(outcome.applied());

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
        load_cursor(repo.heddle_dir(), "acme/widgets")
            .unwrap()
            .after_event_id,
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
        load_cursor(repo.heddle_dir(), "acme/widgets")
            .unwrap()
            .after_event_id,
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
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
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
    assert!(super::discussion_from_payload("turn.appended", &payload, true).is_none());
    assert!(super::discussion_from_payload("turn.appended", &payload, false).is_none());
    assert!(super::discussion_from_payload("discussion.opened", &payload, false).is_none());
}

#[test]
fn dismissed_resolution_parses_from_payload() {
    let event = resolved_event(1, "disc-x");
    let payload = parse_event_payload(&event);
    assert!(matches!(
        payload.resolution,
        HostedResolution::Dismissed { reason } if reason == "done"
    ));
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
        load_cursor(repo.heddle_dir(), "acme/widgets")
            .unwrap()
            .after_event_id,
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
        load_cursor(repo.heddle_dir(), "acme/widgets")
            .unwrap()
            .after_event_id,
        45
    );

    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn bootstrap_none_hits_list_by_state() {
    let (_temp, repo) = seed_repo();
    let mut fixture = CollaborationFixture::default();
    fixture.list = vec![proto_discussion(
        "disc-list",
        &[("turn-open", "from list", 1)],
    )];
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
    assert!(super::discussion_from_payload("discussion.opened", &payload, false).is_none());
}

#[test]
fn fat_append_on_unknown_discussion_is_not_self_contained() {
    let event = appended_event(2, "disc-unknown", "second turn", "turn-2", 2);
    let payload = parse_event_payload(&event);
    assert!(super::discussion_from_payload("turn.appended", &payload, false).is_none());
    assert!(super::discussion_from_payload("turn.appended", &payload, true).is_some());
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
    assert!(super::discussion_from_payload("discussion.opened", &payload, false).is_none());
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
    assert!(super::discussion_from_payload("discussion.resolved", &payload, true).is_none());
    assert!(super::discussion_from_payload("discussion.resolved", &payload, false).is_none());
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
fn unfiltered_authority_cursor_falls_back_to_legacy_repo_path() {
    let temp = TempDir::new().unwrap();
    let legacy = DiscussionEventCursor {
        after_event_id: 7,
        repo_id: "repo-1".into(),
        bootstrapped: true,
    };
    save_cursor(temp.path(), "acme/widgets", &legacy).unwrap();
    let scoped = DiscussionCursorScope {
        authority: "weft.example:443".into(),
        repo_path: "acme/widgets".into(),
        ..DiscussionCursorScope::default()
    };
    assert_eq!(
        load_scoped_cursor(temp.path(), &scoped).unwrap(),
        legacy
    );
    let advanced = DiscussionEventCursor {
        after_event_id: 9,
        repo_id: "repo-1".into(),
        bootstrapped: true,
    };
    save_scoped_cursor(temp.path(), &scoped, &advanced).unwrap();
    assert_eq!(
        load_cursor(temp.path(), "acme/widgets").unwrap().after_event_id,
        7
    );
    assert_eq!(
        load_scoped_cursor(temp.path(), &scoped)
            .unwrap()
            .after_event_id,
        9
    );
}

#[tokio::test]
async fn filtered_wait_does_not_advance_the_unfiltered_watermark() {
    let (_temp, repo) = seed_repo();
    let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
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
        ["disc-res"]
    );

    client.close().await;
    server.await.unwrap();
}
