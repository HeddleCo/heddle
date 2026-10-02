// SPDX-License-Identifier: Apache-2.0
use objects::{
    object::{
        Agent, Attribution, AttributionBasis, AttributionClaim, AttributionEvidenceV1,
        AttributionSource, Blob, Principal, State, Tree, TreeEntry,
    },
    store::{ObjectStore, pack::PackObjectId},
};
use oplog::OpLogBackend;

use super::{
    SnapshotDetails, SnapshotFault, SnapshotSource, prepare_attribution_evidence,
    snapshot_transaction_id, with_forced_revalidation_retries, with_snapshot_fault,
};
use crate::Repository;

fn harness_evidence(harness: &str) -> AttributionEvidenceV1 {
    AttributionEvidenceV1 {
        harness: Some(AttributionClaim::new(
            harness,
            AttributionBasis::Observed,
            AttributionSource::HarnessHook,
        )),
        ..Default::default()
    }
}

fn principal_only() -> Attribution {
    Attribution::human(Principal::new("Capture Test", "capture@example.test"))
}

fn repository() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = crate::init_test_repository(dir.path()).unwrap();
    (dir, repo)
}

fn assert_committed_evidence(repo: &Repository, state: &State, evidence: &AttributionEvidenceV1) {
    let expected = evidence.to_blob().unwrap();
    assert_eq!(state.attribution_evidence, Some(expected.hash()));
    let stored = repo.store().get_blob(&expected.hash()).unwrap().unwrap();
    assert_eq!(stored.content(), expected.content());
    assert_eq!(
        AttributionEvidenceV1::from_blob_with_hash(&stored, expected.hash()).unwrap(),
        *evidence
    );
    let descriptor = repo
        .store()
        .snapshot_commit_descriptor_for_state(&state.id())
        .unwrap()
        .expect("evidence-bearing state has an authoritative commit artifact");
    assert!(
        descriptor
            .object_ids
            .contains(&PackObjectId::StateId(state.id()))
    );
    assert!(
        descriptor
            .object_ids
            .contains(&PackObjectId::Hash(expected.hash()))
    );
    assert!(repo.get_state_signature(&state.id()).unwrap().is_some());
    assert_eq!(repo.head().unwrap(), Some(state.id()));
}

#[test]
fn snapshot_unknown_model_commits_harness_without_inventing_legacy_agent() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "source").unwrap();
    let evidence = harness_evidence("cursor");
    let captured = repo
        .snapshot_with_attribution_evidence_profiled(
            Some("unknown backend".into()),
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap();
    assert!(captured.state.attribution.agent.is_none());
    assert_committed_evidence(&repo, &captured.state, &evidence);
    let reopened = Repository::open(dir.path()).unwrap();
    let state = reopened
        .store()
        .get_state(&captured.state.id())
        .unwrap()
        .unwrap();
    assert_committed_evidence(&reopened, &state, &evidence);
}

#[test]
fn snapshot_contradictory_legacy_agent_fails_before_publication() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "source").unwrap();
    let evidence = harness_evidence("cursor");
    let before = repo.head().unwrap();
    let attribution = Attribution::with_agent(
        Principal::new("Capture Test", "capture@example.test"),
        Agent::new("invented-provider", "invented-model"),
    );
    let error = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            attribution,
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap_err();
    assert!(error.to_string().contains("contradicts"));
    assert_eq!(repo.head().unwrap(), before);
    assert!(
        !repo
            .store()
            .has_blob_locally(&evidence.to_blob().unwrap().hash())
            .unwrap()
    );
}

#[test]
fn snapshot_evidence_survives_worktree_revalidation_retries() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "source").unwrap();
    let evidence = harness_evidence("codex");
    let captured = with_forced_revalidation_retries(2, || {
        repo.snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
    })
    .unwrap();
    assert_committed_evidence(&repo, &captured.state, &evidence);
    assert_eq!(
        repo.oplog()
            .recent(32)
            .unwrap()
            .iter()
            .filter(|entry| {
                matches!(entry.operation, oplog::OpRecord::Snapshot { new_state, .. }
                if new_state == captured.state.id())
            })
            .count(),
        1
    );
}

#[test]
fn supplied_tree_and_blobs_pack_evidence_before_state_publication() {
    for supplied_blobs in [false, true] {
        let (_dir, repo) = repository();
        let source = Blob::new(b"supplied source".to_vec());
        let tree = Tree::from_entries(vec![
            TreeEntry::file("tracked.txt", source.hash(), false).unwrap(),
        ]);
        let evidence = harness_evidence("claude-code");
        let captured = if supplied_blobs {
            repo.snapshot_tree_with_blobs_with_attribution_evidence_profiled(
                tree,
                vec![source.clone()],
                None,
                None,
                principal_only(),
                Some(evidence.clone()),
            )
        } else {
            repo.store().put_blob(&source).unwrap();
            repo.snapshot_tree_with_attribution_evidence_profiled(
                tree,
                None,
                None,
                principal_only(),
                Some(evidence.clone()),
            )
        }
        .unwrap();
        assert_committed_evidence(&repo, &captured.state, &evidence);
        assert!(repo.store().has_blob_locally(&source.hash()).unwrap());
    }
}

#[test]
fn snapshot_precommit_failure_cannot_publish_evidence_or_state() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "source").unwrap();
    let evidence = harness_evidence("cursor");
    let before = repo.head().unwrap();
    let failed = with_snapshot_fault(SnapshotFault::StageBeforeAtomicCommit, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            repo.snapshot_with_attribution_evidence_profiled(
                None,
                None,
                principal_only(),
                Some(evidence.clone()),
                None,
                false,
            )
        }))
    });
    assert!(failed.is_err());
    assert_eq!(repo.head().unwrap(), before);
    assert!(
        !repo
            .store()
            .has_blob_locally(&evidence.to_blob().unwrap().hash())
            .unwrap()
    );
    let captured = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap();
    assert_committed_evidence(&repo, &captured.state, &evidence);
}

#[test]
fn snapshot_committed_pack_recovery_retains_required_evidence() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "source").unwrap();
    let evidence = harness_evidence("cursor");
    let failed = with_snapshot_fault(SnapshotFault::ArtifactCommitBeforeOplogView, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            repo.snapshot_with_attribution_evidence_profiled(
                None,
                None,
                principal_only(),
                Some(evidence.clone()),
                None,
                false,
            )
        }))
    });
    assert!(failed.is_err());
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    let recovered = reopened
        .store()
        .get_state(&reopened.head().unwrap().unwrap())
        .unwrap()
        .unwrap();
    assert_committed_evidence(&reopened, &recovered, &evidence);
}

#[test]
fn merge_snapshot_packs_evidence_and_binds_signature() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "first").unwrap();
    let first = repo.snapshot(Some("first".into()), None).unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "second").unwrap();
    repo.snapshot(Some("second".into()), None).unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "merged").unwrap();
    let evidence = harness_evidence("cursor");
    let merged = repo
        .snapshot_merge_with_attribution_evidence(
            &first.id(),
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            Some(first.id()),
            false,
        )
        .unwrap();
    assert_eq!(merged.parents.len(), 2);
    assert_committed_evidence(&repo, &merged, &evidence);
}

#[test]
fn snapshot_transaction_and_state_identity_commit_evidence_digest() {
    let (_dir, repo) = repository();
    let evidence = harness_evidence("cursor");
    let replacement = harness_evidence("codex");
    let tree = Tree::new();
    let source = SnapshotSource::SuppliedTree(tree.clone());
    let head = repo.head_ref().unwrap();
    let mut details = SnapshotDetails {
        intent: None,
        confidence: None,
        attribution: principal_only(),
        attribution_evidence: None,
        lineage: Vec::new(),
    };
    let legacy_transaction = snapshot_transaction_id(&repo, &source, &details, &head, None);
    details.attribution_evidence =
        prepare_attribution_evidence(&details.attribution, Some(evidence.clone())).unwrap();
    let original_transaction = snapshot_transaction_id(&repo, &source, &details, &head, None);
    assert_ne!(legacy_transaction, original_transaction);
    assert_eq!(
        original_transaction,
        snapshot_transaction_id(&repo, &source, &details, &head, None)
    );
    details.attribution_evidence =
        prepare_attribution_evidence(&details.attribution, Some(replacement.clone())).unwrap();
    assert_ne!(
        original_transaction,
        snapshot_transaction_id(&repo, &source, &details, &head, None)
    );
    let original_blob = evidence.to_blob().unwrap();
    let replacement_blob = replacement.to_blob().unwrap();
    let original = State::new_snapshot(tree.hash(), vec![], principal_only())
        .with_attribution_evidence(original_blob.hash());
    let changed = original
        .clone()
        .with_attribution_evidence(replacement_blob.hash());
    assert_ne!(original.id(), changed.id());
    assert!(
        AttributionEvidenceV1::from_blob_with_hash(&replacement_blob, original_blob.hash())
            .is_err()
    );
}

#[test]
fn replay_refuses_missing_or_contradictory_evidence() {
    let (_dir, repo) = repository();
    let evidence = harness_evidence("cursor");
    let blob = evidence.to_blob().unwrap();
    let tree = Tree::new();
    let state = State::new_snapshot(tree.hash(), vec![], principal_only())
        .with_attribution_evidence(blob.hash());
    let mutation = super::SnapshotMutation::new(
        &repo,
        SnapshotSource::SuppliedTree(tree),
        SnapshotDetails {
            intent: None,
            confidence: None,
            attribution: principal_only(),
            attribution_evidence: Some(blob.clone()),
            lineage: vec![],
        },
        None,
        repo.head_ref().unwrap(),
        None,
        false,
        vec![],
    );
    let error = mutation
        .validate_replayed_attribution_evidence(&state)
        .unwrap_err();
    assert!(error.to_string().contains("evidence missing"));
    repo.store().put_blob(&blob).unwrap();
    mutation
        .validate_replayed_attribution_evidence(&state)
        .unwrap();
    let changed = state
        .clone()
        .with_attribution_evidence(harness_evidence("codex").to_blob().unwrap().hash());
    assert!(
        mutation
            .validate_replayed_attribution_evidence(&changed)
            .is_err()
    );
    let mut contradictory = state;
    contradictory.attribution.agent = Some(Agent::new("provider", "model"));
    assert!(
        mutation
            .validate_replayed_attribution_evidence(&contradictory)
            .is_err()
    );
}

#[test]
fn in_progress_merge_capture_keeps_evidence() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("tracked.txt"), "first").unwrap();
    let first = repo.snapshot(Some("first".into()), None).unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "second").unwrap();
    let second = repo.snapshot(Some("second".into()), None).unwrap();
    repo.merge_state_manager()
        .start(second.id(), first.id(), Some(first.id()), vec![], None)
        .unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "merged").unwrap();
    let evidence = harness_evidence("cursor");
    let captured = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap();
    assert_eq!(captured.state.parents, vec![second.id(), first.id()]);
    assert_committed_evidence(&repo, &captured.state, &evidence);
    assert!(repo.merge_state_manager().load().unwrap().is_none());
}

fn bound_contributor(
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
    model: &str,
) -> objects::object::AttributionOperation {
    use objects::object::{
        AttributionFileChange, AttributionOperation, AttributionOperationIdentity,
        AttributionOperationResolution, AttributionScope, ModelAttribution,
    };
    AttributionOperation {
        identity: AttributionOperationIdentity {
            selected: ModelAttribution {
                model: Some(AttributionClaim::new(
                    model,
                    AttributionBasis::RequestReported,
                    AttributionSource::HarnessHook,
                )),
                ..Default::default()
            },
            scope: AttributionScope {
                tool_call_id: Some(format!("tool-{model}")),
                ..Default::default()
            },
            ..Default::default()
        },
        changes: vec![AttributionFileChange {
            path: path.into(),
            before: before.map(|v| Blob::from_slice(v.as_bytes()).hash()),
            after: after.map(|v| Blob::from_slice(v.as_bytes()).hash()),
        }],
        resolution: AttributionOperationResolution::ContentBound,
    }
}

#[test]
fn snapshot_bound_multi_model_nested_chain_commits_exact_evidence() {
    let (dir, repo) = repository();
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::write(dir.path().join("src/nested/file.txt"), "final").unwrap();
    let evidence = AttributionEvidenceV1 {
        operations: vec![
            bound_contributor("src/nested/file.txt", None, Some("intermediate"), "model-a"),
            bound_contributor(
                "src/nested/file.txt",
                Some("intermediate"),
                Some("final"),
                "model-b",
            ),
        ],
        ..Default::default()
    };
    let captured = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap();
    assert_committed_evidence(&repo, &captured.state, &evidence);
    assert!(captured.state.attribution.agent.is_none());
    assert_eq!(crate::attribution_model(None, Some(&evidence)), None);
}

#[test]
fn snapshot_bound_terminal_race_fails_before_state_ref_or_evidence_publication() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("file.txt"), "raced content").unwrap();
    let evidence = AttributionEvidenceV1 {
        operations: vec![bound_contributor(
            "file.txt",
            None,
            Some("observed content"),
            "model-a",
        )],
        ..Default::default()
    };
    let before = repo.head().unwrap();
    let failure = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap_err();
    assert!(failure.to_string().contains("captured content"));
    assert_eq!(repo.head().unwrap(), before);
    assert!(
        !repo
            .store()
            .has_blob_locally(&evidence.to_blob().unwrap().hash())
            .unwrap()
    );
}

#[test]
fn snapshot_bound_first_parent_mismatch_fails_and_exact_deletion_succeeds() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("file.txt"), "parent content").unwrap();
    let parent = repo.snapshot(None, None).unwrap();
    std::fs::remove_file(dir.path().join("file.txt")).unwrap();
    let mut evidence = AttributionEvidenceV1 {
        operations: vec![bound_contributor(
            "file.txt",
            Some("wrong parent"),
            None,
            "model-a",
        )],
        ..Default::default()
    };
    let failure = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap_err();
    assert!(failure.to_string().contains("first-parent"));
    assert_eq!(repo.head().unwrap(), Some(parent.id()));
    evidence.operations[0] = bound_contributor("file.txt", Some("parent content"), None, "model-a");
    let captured = repo
        .snapshot_with_attribution_evidence_profiled(
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            None,
            false,
        )
        .unwrap();
    assert_committed_evidence(&repo, &captured.state, &evidence);
}

#[test]
fn merge_bound_transition_uses_first_parent_not_other_merge_parent() {
    let (dir, repo) = repository();
    std::fs::write(dir.path().join("file.txt"), "other parent").unwrap();
    let other = repo.snapshot(None, None).unwrap();
    std::fs::write(dir.path().join("file.txt"), "first parent").unwrap();
    let first = repo.snapshot(None, None).unwrap();
    std::fs::write(dir.path().join("file.txt"), "merged content").unwrap();
    let mut evidence = AttributionEvidenceV1 {
        operations: vec![bound_contributor(
            "file.txt",
            Some("other parent"),
            Some("merged content"),
            "model-a",
        )],
        ..Default::default()
    };
    let failure = repo
        .snapshot_merge_with_attribution_evidence(
            &other.id(),
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            Some(other.id()),
            false,
        )
        .unwrap_err();
    assert!(failure.to_string().contains("first-parent"));
    assert_eq!(repo.head().unwrap(), Some(first.id()));
    evidence.operations[0] = bound_contributor(
        "file.txt",
        Some("first parent"),
        Some("merged content"),
        "model-a",
    );
    let merged = repo
        .snapshot_merge_with_attribution_evidence(
            &other.id(),
            None,
            None,
            principal_only(),
            Some(evidence.clone()),
            Some(other.id()),
            false,
        )
        .unwrap();
    assert_eq!(merged.parents, vec![first.id(), other.id()]);
    assert_committed_evidence(&repo, &merged, &evidence);
}

#[test]
fn replay_revalidates_bound_contributors_against_committed_tree() {
    let (_dir, repo) = repository();
    let actual = Blob::from_slice(b"actual content");
    let tree = Tree::from_entries(vec![
        TreeEntry::file("file.txt", actual.hash(), false).unwrap(),
    ]);
    repo.store().put_blob(&actual).unwrap();
    repo.store().put_tree(&tree).unwrap();
    let evidence = AttributionEvidenceV1 {
        operations: vec![bound_contributor(
            "file.txt",
            None,
            Some("different content"),
            "model-a",
        )],
        ..Default::default()
    };
    let blob = evidence.to_blob().unwrap();
    repo.store().put_blob(&blob).unwrap();
    let state = State::new_snapshot(tree.hash(), vec![], principal_only())
        .with_attribution_evidence(blob.hash());
    let mutation = super::SnapshotMutation::new(
        &repo,
        SnapshotSource::SuppliedTree(tree),
        SnapshotDetails {
            intent: None,
            confidence: None,
            attribution: principal_only(),
            attribution_evidence: Some(blob),
            lineage: vec![],
        },
        None,
        repo.head_ref().unwrap(),
        None,
        false,
        vec![],
    );
    let failure = mutation
        .validate_replayed_attribution_evidence(&state)
        .unwrap_err();
    assert!(failure.to_string().contains("captured content"));
}
