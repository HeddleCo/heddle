use super::*;
use serde_json::json;
fn event(model: Option<&str>, turn: &str, tool: &str) -> IdentityCursor {
    crate::codex_cursor_patch(
        &json!({"session_id":"session","turn_id":turn,"tool_use_id":tool,"model":model}),
    )
}
fn frozen(root: &Path) -> AttributionEvidenceV1 {
    let mut evidence = AttributionEvidenceV1::default();
    freeze(root, &mut evidence).unwrap();
    evidence
}
fn operation(root: &Path, before: &IdentityCursor, path: &str, bytes: &str) {
    record_operation_event(root, before, OperationEventPhase::Before, &[path.into()]).unwrap();
    fs::write(root.join(path), bytes).unwrap();
    record_operation_event(root, before, OperationEventPhase::After, &[path.into()]).unwrap();
}
#[test]
fn mixed_turns_freeze_each_producing_model_and_bind_one_capture() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo::Repository::init_default(dir.path()).unwrap();
    operation(
        dir.path(),
        &event(Some("model-one"), "turn-one", "tool-one"),
        "file",
        "one",
    );
    operation(
        dir.path(),
        &event(Some("model-two"), "turn-two", "tool-two"),
        "file",
        "two",
    );
    let mut evidence = frozen(dir.path());
    bind(&repo, &mut evidence).unwrap();
    assert_eq!(evidence.operations.len(), 2);
    assert_eq!(
        evidence.operations[0]
            .identity
            .selected
            .model
            .as_ref()
            .unwrap()
            .value,
        "model-one"
    );
    assert_eq!(
        evidence.operations[1]
            .identity
            .selected
            .model
            .as_ref()
            .unwrap()
            .value,
        "model-two"
    );
    assert!(
        evidence
            .operations
            .iter()
            .all(|op| op.resolution == AttributionOperationResolution::ContentBound)
    );
    assert!(evidence.legacy_agent().is_none());
    evidence.validate().unwrap();
}
#[test]
fn delayed_post_uses_original_operation_and_duplicate_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let first = event(Some("first"), "old", "tool");
    record_operation_event(
        dir.path(),
        &first,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    crate::stamp_identity_cursor(dir.path(), &event(Some("later"), "new", "other")).unwrap();
    fs::write(dir.path().join("file"), "one").unwrap();
    for _ in 0..2 {
        record_operation_event(
            dir.path(),
            &first,
            OperationEventPhase::After,
            &["file".into()],
        )
        .unwrap();
    }
    let evidence = frozen(dir.path());
    assert_eq!(evidence.operations.len(), 1);
    assert_eq!(
        evidence.operations[0]
            .identity
            .selected
            .model
            .as_ref()
            .unwrap()
            .value,
        "first"
    );
}
#[test]
fn out_of_order_and_missing_ids_are_explicitly_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    record_operation_event(
        dir.path(),
        &event(Some("m"), "t", "call"),
        OperationEventPhase::After,
        &["file".into()],
    )
    .unwrap();
    let missing = crate::codex_cursor_patch(&json!({"session_id":"session","model":"other"}));
    record_operation_event(
        dir.path(),
        &missing,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    let evidence = frozen(dir.path());
    assert!(evidence.operations_incomplete);
    assert_eq!(evidence.operations.len(), 2);
    assert!(
        evidence
            .operations
            .iter()
            .all(|op| op.resolution == AttributionOperationResolution::Unresolved)
    );
}
#[test]
fn overlapping_actors_cannot_claim_exclusive_file_transition() {
    let dir = tempfile::tempdir().unwrap();
    let a = event(Some("a"), "turn-a", "a");
    let b = event(Some("b"), "turn-b", "b");
    for e in [&a, &b] {
        record_operation_event(dir.path(), e, OperationEventPhase::Before, &["file".into()])
            .unwrap();
    }
    fs::write(dir.path().join("file"), "mixed").unwrap();
    for e in [&b, &a] {
        record_operation_event(dir.path(), e, OperationEventPhase::After, &["file".into()])
            .unwrap();
    }
    assert!(
        frozen(dir.path())
            .operations
            .iter()
            .all(|op| op.resolution == AttributionOperationResolution::Unresolved)
    );
}
#[test]
fn conflict_preserves_both_models_without_picking_a_winner() {
    let dir = tempfile::tempdir().unwrap();
    let a = event(Some("a"), "t", "tool");
    let b = event(Some("b"), "t", "tool");
    record_operation_event(
        dir.path(),
        &a,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    fs::write(dir.path().join("file"), "one").unwrap();
    record_operation_event(dir.path(), &b, OperationEventPhase::After, &["file".into()]).unwrap();
    let evidence = frozen(dir.path());
    assert_eq!(evidence.operations.len(), 2);
    assert!(
        evidence
            .operations
            .iter()
            .all(|op| op.resolution == AttributionOperationResolution::Unresolved)
    );
    let models: std::collections::BTreeSet<_> = evidence
        .operations
        .iter()
        .map(|op| op.identity.selected.model.as_ref().unwrap().value.as_str())
        .collect();
    assert_eq!(models, std::collections::BTreeSet::from(["a", "b"]));
}
#[test]
fn overlapping_collectors_deduplicate_causal_fact_and_retain_methods() {
    let dir = tempfile::tempdir().unwrap();
    let e = event(Some("m"), "t", "tool");
    for method in [
        AttributionCollectionMethod::Hook,
        AttributionCollectionMethod::EventStream,
    ] {
        record_with_method(
            dir.path(),
            &e,
            OperationEventPhase::Before,
            &["file".into()],
            method,
        )
        .unwrap();
    }
    fs::write(dir.path().join("file"), "one").unwrap();
    for method in [
        AttributionCollectionMethod::Hook,
        AttributionCollectionMethod::EventStream,
    ] {
        record_with_method(
            dir.path(),
            &e,
            OperationEventPhase::After,
            &["file".into()],
            method,
        )
        .unwrap();
    }
    let evidence = frozen(dir.path());
    assert_eq!(evidence.operations.len(), 1);
    assert_eq!(evidence.operations[0].identity.collection_methods.len(), 2);
}
#[test]
fn opaque_and_rejected_tools_never_claim_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    let e = event(Some("m"), "t", "tool");
    record_operation_event(dir.path(), &e, OperationEventPhase::Before, &[]).unwrap();
    fs::write(dir.path().join("file"), "shell edit").unwrap();
    record_operation_event(dir.path(), &e, OperationEventPhase::After, &[]).unwrap();
    let e = event(Some("m"), "t", "failed");
    record_operation_event(
        dir.path(),
        &e,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    record_operation_event(
        dir.path(),
        &e,
        OperationEventPhase::Failed,
        &["file".into()],
    )
    .unwrap();
    assert!(
        frozen(dir.path())
            .operations
            .iter()
            .all(|op| op.resolution == AttributionOperationResolution::Unresolved)
    );
}
#[cfg(unix)]
#[test]
fn symlink_targets_are_never_read_or_bound() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("private"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path().join("private"), dir.path().join("file")).unwrap();
    let e = event(Some("m"), "t", "tool");
    for phase in [OperationEventPhase::Before, OperationEventPhase::After] {
        record_operation_event(dir.path(), &e, phase, &["file".into()]).unwrap();
    }
    let evidence = frozen(dir.path());
    assert_eq!(
        evidence.operations[0].resolution,
        AttributionOperationResolution::Unresolved
    );
    assert!(evidence.operations[0].changes.is_empty());
}
#[test]
fn acknowledgment_keeps_new_events_and_failed_capture_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    operation(dir.path(), &event(Some("a"), "a", "a"), "a", "a");
    let captured = frozen(dir.path());
    assert_eq!(frozen(dir.path()).operations, captured.operations);
    operation(dir.path(), &event(Some("b"), "b", "b"), "b", "b");
    acknowledge(dir.path(), &captured).unwrap();
    let next = frozen(dir.path());
    assert_eq!(next.operations.len(), 1);
    assert_eq!(
        next.operations[0].identity.scope.tool_call_id.as_deref(),
        Some("b")
    );
}
#[test]
fn unknown_model_remains_unknown_despite_current_cursor() {
    let dir = tempfile::tempdir().unwrap();
    crate::stamp_identity_cursor(dir.path(), &event(Some("current"), "other", "other")).unwrap();
    operation(dir.path(), &event(None, "actual", "actual"), "file", "one");
    assert!(
        frozen(dir.path()).operations[0]
            .identity
            .selected
            .model
            .is_none()
    );
}
#[test]
fn rollback_and_deleted_intermediate_edits_are_not_surviving_contributors() {
    for delete in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo::Repository::init_default(dir.path()).unwrap();
        fs::write(dir.path().join("file"), "original").unwrap();
        repo.snapshot_with_attribution(
            Some("base".into()),
            None,
            objects::object::Attribution::human(objects::object::Principal::new(
                "Test",
                "test@example.test",
            )),
        )
        .unwrap();
        operation(dir.path(), &event(Some("a"), "a", "a"), "file", "transient");
        let e = event(Some("b"), "b", "b");
        record_operation_event(
            dir.path(),
            &e,
            OperationEventPhase::Before,
            &["file".into()],
        )
        .unwrap();
        if delete {
            fs::remove_file(dir.path().join("file")).unwrap();
        } else {
            fs::write(dir.path().join("file"), "original").unwrap();
        }
        record_operation_event(dir.path(), &e, OperationEventPhase::After, &["file".into()])
            .unwrap();
        let mut evidence = frozen(dir.path());
        bind(&repo, &mut evidence).unwrap();
        assert!(
            evidence
                .operations
                .iter()
                .all(|op| op.resolution == AttributionOperationResolution::Unresolved)
        );
    }
}
#[test]
fn path_conflicts_and_invalid_post_paths_are_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let e = event(Some("m"), "t", "tool");
    record_operation_event(dir.path(), &e, OperationEventPhase::Before, &["one".into()]).unwrap();
    record_operation_event(dir.path(), &e, OperationEventPhase::Before, &["two".into()]).unwrap();
    fs::write(dir.path().join("one"), "one").unwrap();
    record_operation_event(
        dir.path(),
        &e,
        OperationEventPhase::After,
        &["../outside".into()],
    )
    .unwrap();
    assert_eq!(
        frozen(dir.path()).operations[0].resolution,
        AttributionOperationResolution::Unresolved
    );
}
#[test]
fn replay_after_publication_cannot_attribute_future_edits_to_old_tool() {
    let dir = tempfile::tempdir().unwrap();
    let e = event(Some("old-model"), "old", "old");
    operation(dir.path(), &e, "file", "old");
    let captured = frozen(dir.path());
    acknowledge(dir.path(), &captured).unwrap();
    record_operation_event(
        dir.path(),
        &e,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    fs::write(dir.path().join("file"), "new unrelated edit").unwrap();
    record_operation_event(dir.path(), &e, OperationEventPhase::After, &["file".into()]).unwrap();
    assert!(frozen(dir.path()).operations.is_empty());
}
#[test]
fn replay_index_keeps_old_ids_after_many_later_operations() {
    let dir = tempfile::tempdir().unwrap();
    let e = event(Some("old"), "old", "old");
    operation(dir.path(), &e, "file", "old");
    let evidence = frozen(dir.path());
    acknowledge(dir.path(), &evidence).unwrap();
    let mut database = replay_database(dir.path()).unwrap();
    let transaction = database.transaction().unwrap();
    for index in 0..2048 {
        let id = ContentHash::compute(format!("later-{index}").as_bytes());
        transaction
            .execute(
                "INSERT INTO seen (id) VALUES (?1)",
                [id.as_bytes().as_slice()],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(database);
    record_operation_event(
        dir.path(),
        &e,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    assert!(frozen(dir.path()).operations.is_empty());
}

#[test]
fn metadata_observation_does_not_close_or_resnapshot_tool() {
    let dir = tempfile::tempdir().unwrap();
    let before = event(None, "turn", "tool");
    fs::write(dir.path().join("file"), "before").unwrap();
    record_operation_event(
        dir.path(),
        &before,
        OperationEventPhase::Before,
        &["file".into()],
    )
    .unwrap();
    let observed = event(Some("response-model"), "turn", "tool");
    record_with_method(
        dir.path(),
        &observed,
        OperationEventPhase::Observe,
        &[],
        AttributionCollectionMethod::EventStream,
    )
    .unwrap();
    fs::write(dir.path().join("file"), "after").unwrap();
    record_operation_event(
        dir.path(),
        &before,
        OperationEventPhase::After,
        &["file".into()],
    )
    .unwrap();
    let evidence = frozen(dir.path());
    assert_eq!(evidence.operations.len(), 1);
    let operation = &evidence.operations[0];
    assert_eq!(
        operation.resolution,
        AttributionOperationResolution::ContentBound
    );
    assert_eq!(
        operation.identity.selected.model.as_ref().unwrap().value,
        "response-model"
    );
    assert_eq!(
        operation.changes[0].before,
        Some(Blob::new(b"before".to_vec()).hash())
    );
    assert_eq!(
        operation.changes[0].after,
        Some(Blob::new(b"after".to_vec()).hash())
    );
    assert_eq!(operation.identity.collection_methods.len(), 2);
}

#[test]
fn late_metadata_conflict_keeps_completed_operation_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let before = event(Some("first"), "turn", "tool");
    operation(dir.path(), &before, "file", "after");
    let late = event(Some("second"), "turn", "tool");
    record_with_method(
        dir.path(),
        &late,
        OperationEventPhase::Observe,
        &[],
        AttributionCollectionMethod::EventStream,
    )
    .unwrap();
    record_with_method(
        dir.path(),
        &before,
        OperationEventPhase::Observe,
        &[],
        AttributionCollectionMethod::EventStream,
    )
    .unwrap();
    let evidence = frozen(dir.path());
    assert_eq!(evidence.operations.len(), 2);
    assert!(evidence.operations_incomplete);
    assert!(
        evidence
            .operations
            .iter()
            .all(|op| op.resolution == AttributionOperationResolution::Unresolved)
    );
}

#[test]
fn metadata_without_tool_boundary_does_not_create_a_producer() {
    let dir = tempfile::tempdir().unwrap();
    let e = event(Some("model"), "turn", "tool");
    record_with_method(
        dir.path(),
        &e,
        OperationEventPhase::Observe,
        &[],
        AttributionCollectionMethod::EventStream,
    )
    .unwrap();
    assert!(frozen(dir.path()).operations.is_empty());
}
