use api::heddle::api::v2alpha1::{
    DecideRunPermissionRequest, RecordRef, SpoolRef, control_run_request::Action,
    operation_record::State,
};

use super::*;
fn fixture() -> (tempfile::TempDir, RunStore, RunRecord) {
    let directory = tempfile::tempdir().expect("directory");
    let store = RunStore::open(directory.path()).expect("store");
    let run = store
        .put_run(RunRecord {
            r#ref: Some(RecordRef {
                id: "run-1".into(),
                spool: Some(SpoolRef {
                    id: uuid::Uuid::from_u128(1).to_string(),
                }),
            }),
            state: State::Running as i32,
            supported_controls: vec![Action::Pause as i32, Action::Resume as i32],
            ..Default::default()
        })
        .expect("register run");
    (directory, store, run)
}
#[test]
fn controls_preserve_arrival_order_and_need_real_harness_acknowledgment() {
    let (_directory, store, run) = fixture();
    let pause = ControlRunRequest {
        client_operation_id: "z-first".into(),
        run: run.r#ref.clone(),
        expected_version: run.version.clone(),
        action: Action::Pause as i32,
        ..Default::default()
    };
    let resume = ControlRunRequest {
        client_operation_id: "a-second".into(),
        action: Action::Resume as i32,
        ..pause.clone()
    };
    store
        .enqueue_control(&pause, "actor")
        .expect("pause request");
    store
        .enqueue_control(&resume, "actor")
        .expect("resume request");
    store
        .enqueue_control(&pause, "actor")
        .expect("retry exact request");
    assert_eq!(
        store.run("run-1").expect("read").expect("run").state,
        State::Running as i32,
        "queued request does not fake execution"
    );
    let commands = store.pending_controls("run-1", 10).expect("queue");
    assert_eq!(
        commands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["z-first", "a-second"]
    );
    assert!(
        store.enqueue_control(&pause, "other actor").is_err(),
        "retry binds principal"
    );
    let mut paused = run.clone();
    paused.state = State::Paused as i32;
    assert!(
        store.complete_control("a-second", &paused).is_err(),
        "cannot acknowledge out of order"
    );
    store
        .complete_control("z-first", &paused)
        .expect("actual harness pause");
    assert_eq!(
        store.run("run-1").expect("read").expect("run").state,
        State::Paused as i32
    );
    assert_eq!(
        store.pending_controls("run-1", 10).expect("pending").len(),
        1
    );
    store
        .complete_control("z-first", &run)
        .expect("duplicate ack does not overwrite state");
    assert_eq!(
        store.run("run-1").expect("read").expect("run").state,
        State::Paused as i32
    );
    let mut unsupported = resume;
    unsupported.client_operation_id = "steer".into();
    unsupported.action = Action::Steer as i32;
    unsupported.instruction = "change".into();
    unsupported.expected_version = store.run("run-1").expect("read").expect("run").version;
    assert!(
        store
            .enqueue_control(&unsupported, "actor")
            .expect_err("unsupported")
            .to_string()
            .contains("does not support")
    );
}
#[test]
fn permission_decisions_bind_pending_digest_and_cannot_be_reused() {
    let (_directory, store, run) = fixture();
    let request = DecideRunPermissionRequest {
        client_operation_id: "permission-1".into(),
        run: run.r#ref,
        permission_id: "execute".into(),
        request_digest: vec![7; 32],
        allow: true,
    };
    assert!(
        store.decide_permission(&request, "actor").is_err(),
        "unknown prompt is never approvable"
    );
    store
        .request_permission("run-1", "execute", &[7; 32])
        .expect("pending request");
    let mut wrong = request.clone();
    wrong.request_digest = vec![8; 32];
    assert!(store.decide_permission(&wrong, "actor").is_err());
    store
        .decide_permission(&request, "actor")
        .expect("decide exact pending action");
    store
        .decide_permission(&request, "actor")
        .expect("exact retry");
    assert_eq!(
        store
            .permission_decision("run-1", "execute", &[7; 32])
            .expect("decision"),
        Some(true)
    );
    wrong = request;
    wrong.client_operation_id = "permission-2".into();
    wrong.allow = false;
    assert!(
        store.decide_permission(&wrong, "actor").is_err(),
        "another operation cannot replace approval"
    );
}

#[test]
fn timeline_is_immutable_and_shares_bounded_filtered_pagination() {
    let (_directory, store, run) = fixture();
    let event = TimelineRecord {
        r#ref: Some(RecordRef {
            id: "event".into(),
            spool: run.r#ref.as_ref().expect("reference").spool.clone(),
        }),
        run: run.r#ref.clone(),
        position: 0,
        kind: "started".into(),
        summary: "Run started".into(),
        ..Default::default()
    };
    store.put_timeline(&event).expect("event");
    store.put_timeline(&event).expect("exact retry");
    let mut changed = event.clone();
    changed.summary = "changed".into();
    assert!(
        store
            .put_timeline(&changed)
            .expect_err("immutable")
            .to_string()
            .contains("reused")
    );
    assert_eq!(
        store.last_timeline_position("run-1").expect("position"),
        Some(0)
    );
    let first = store
        .observation_page("", 1, &[], &[], true)
        .expect("first");
    assert_eq!(first.len(), 1);
    assert!(matches!(first[0].1, RunObservation::Run(_)));
    let next = store
        .observation_page(&first[0].0, 1, &[], &[], true)
        .expect("next");
    assert_eq!(next.len(), 1);
    assert!(matches!(next[0].1, RunObservation::Timeline(_)));
    assert!(
        store
            .observation_page("", 1, &["different".into()], &[], true)
            .expect("filter")
            .is_empty()
    );
    assert_eq!(
        store
            .observation_page("", 5, &[], &[], false)
            .expect("no timeline")
            .len(),
        1
    );
}

#[test]
fn pending_permission_is_observable_versioned_closed_and_expiring() {
    use api::heddle::api::v2alpha1::RunPermission;
    let (_directory, store, run) = fixture();
    let mut permission = RunPermission {
        id: "exact-intent".into(),
        request_digest: vec![8; 32],
        harness: "claude".into(),
        tool_name: "Read".into(),
        canonical_input_json: br#"{"tool_input":{"file_path":"README.md"}}"#.to_vec(),
        expires_at: Some(Default::default()),
    };
    permission.expires_at.as_mut().expect("deadline").seconds = chrono::Utc::now().timestamp() + 60;
    store
        .request_permission_record("run-1", &permission)
        .expect("pending intent");
    let pending = store.run("run-1").expect("read").expect("run");
    assert_ne!(pending.version, run.version);
    assert_eq!(pending.pending_permissions, [permission.clone()]);
    let refreshed = store.put_run(run.clone()).expect("harness report refresh");
    assert_eq!(
        refreshed.pending_permissions,
        [permission.clone()],
        "report refresh retains still-pending intent"
    );
    store
        .close_permission("run-1", &permission.id, &permission.request_digest)
        .expect("hook exited");
    assert!(
        store
            .run("run-1")
            .expect("read")
            .expect("run")
            .pending_permissions
            .is_empty()
    );
    let decide = DecideRunPermissionRequest {
        client_operation_id: "closed-decision".into(),
        run: run.r#ref.clone(),
        permission_id: permission.id.clone(),
        request_digest: permission.request_digest.clone(),
        allow: true,
    };
    assert!(
        store
            .decide_permission(&decide, "actor")
            .expect_err("closed")
            .to_string()
            .contains("closed")
    );
    let mut expiring = permission;
    expiring.id = "expires".into();
    expiring.expires_at.as_mut().expect("expiry").seconds = chrono::Utc::now().timestamp() + 1;
    store
        .request_permission_record("run-1", &expiring)
        .expect("short prompt");
    let before = store.run("run-1").expect("read").expect("run").version;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let expired = store.run("run-1").expect("read").expect("run");
    assert!(expired.pending_permissions.is_empty());
    assert_ne!(expired.version, before);
    assert_eq!(
        store
            .permission_decision("run-1", &expiring.id, &expiring.request_digest)
            .expect("decision"),
        None
    );
    assert!(
        store
            .decide_permission(
                &DecideRunPermissionRequest {
                    client_operation_id: "expired-decision".into(),
                    permission_id: expiring.id,
                    ..decide
                },
                "actor"
            )
            .expect_err("expired")
            .to_string()
            .contains("expired")
    );
}

#[test]
fn harness_binding_is_indexed_exclusive_and_does_not_create_stores_on_read() {
    let empty = tempfile::tempdir().expect("empty checkout");
    assert!(
        RunStore::open_existing(empty.path())
            .expect("read absent")
            .is_none()
    );
    assert_eq!(
        std::fs::read_dir(empty.path()).expect("directory").count(),
        0
    );
    let (directory, store, mut run) = fixture();
    store
        .bind_harness("claude-code:session:first", "run-1", true)
        .expect("bind first");
    run.r#ref.as_mut().expect("reference").id = "run-2".into();
    store.put_run(run).expect("second agent");
    store
        .bind_harness("claude-code:agent:second", "run-2", true)
        .expect("bind independent agent");
    let error = store
        .bind_harness("claude-code:session:first", "run-2", true)
        .expect_err("cannot steal active identity");
    assert!(error.to_string().contains("another active run"), "{error}");
    let reopened = RunStore::open_existing(directory.path())
        .expect("reopen")
        .expect("existing");
    assert_eq!(
        reopened
            .harness_run("claude-code:session:first")
            .expect("first"),
        Some("run-1".into())
    );
    assert_eq!(
        reopened
            .harness_run("claude-code:agent:second")
            .expect("second"),
        Some("run-2".into())
    );
    store
        .bind_harness("claude-code:session:first", "run-1", false)
        .expect("close first");
    assert!(
        reopened
            .harness_run("claude-code:session:first")
            .expect("closed")
            .is_none()
    );
    store
        .bind_harness("claude-code:session:first", "run-2", true)
        .expect("reuse closed identity");
    assert_eq!(
        reopened
            .harness_run("claude-code:session:first")
            .expect("new binding"),
        Some("run-2".into())
    );
}
