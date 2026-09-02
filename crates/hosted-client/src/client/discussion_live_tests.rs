// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{RepoEvent, RepoEventKind, StateId as ProtoStateId};
use objects::object::{Attribution, Principal};
use repo::{CollaborationStore, Repository};
use tempfile::TempDir;

use super::{
    DiscussionEventCursor, DiscussionEventOutcome, bootstrap_discussions, consume_discussion_event,
    is_discussion_event, load_cursor, parse_event_payload, save_cursor, subscribe_request,
};
use crate::hosted_runtime::hosted::HostedResolution;

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
fn payload_parser_accepts_nested_discussion_and_turn_objects() {
    let event = RepoEvent {
        event_type: "turn.appended".into(),
        payload_json: serde_json::json!({
            "discussion": { "id": "disc-9", "file": "a.rs", "symbol": "f" },
            "turn": {
                "id": "turn-9",
                "turn_seq": 2,
                "body": "second",
                "author_name": "bob",
                "posted_at": 9
            }
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
    assert!(super::discussion_from_payload("turn.appended", &payload).is_none());
    assert!(super::discussion_from_payload(
        "discussion.opened",
        &payload
    )
    .is_some());
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
