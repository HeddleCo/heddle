// SPDX-License-Identifier: Apache-2.0
use crypto::{Ed25519Signer, Signer};
use objects::object::{
    Attribution, CollaborationAnchor, CollaborationIdempotencyKey, CollaborationOperationBodyV1,
    DiscussionRecordId, DiscussionTurnV1, Principal, Tree, VisibilityTier,
};
use tempfile::TempDir;

use super::*;
use crate::Repository;

fn author() -> Attribution {
    Attribution::human(Principal::new("Agent", "agent@example.test"))
}

#[test]
fn genesis_creation_and_reopen_share_the_same_record_bound() {
    let (_dir, repository, mut genesis, _signer, _replica) = setup();
    genesis.intent = "x".repeat(objects::object::thread_replication::MAX_OPERATION_BYTES / 2);
    let bytes = genesis.encode().expect("bounded genesis");
    assert_eq!(
        ThreadGenesis::decode(&bytes).expect("reopen encoded genesis"),
        genesis
    );
    genesis.intent = "x".repeat(objects::object::thread_replication::MAX_OPERATION_BYTES);
    assert!(
        genesis.encode().is_err(),
        "creation must not emit a record that decode rejects"
    );
    assert!(
        genesis.id().is_err(),
        "an unpersistable genesis must not acquire a Thread ID"
    );
    assert!(ThreadReplica::open(repository.heddle_dir(), &genesis).is_err());
}
fn setup() -> (
    TempDir,
    Repository,
    ThreadGenesis,
    Ed25519Signer,
    ThreadReplica,
) {
    let temp = TempDir::new().expect("repo directory");
    let repo = Repository::init_default(temp.path()).expect("native repository");
    let signer = Ed25519Signer::generate().expect("test publisher");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "01980000-0000-7000-8000-000000000001".into(),
        parent: None,
        base: repo.head().expect("head").expect("initial state"),
        name: "shared".into(),
        intent: "two agents".into(),
        creator: signer.public_key().try_into().expect("key"),
        nonce: vec![1],
    };
    let replica = ThreadReplica::open(repo.heddle_dir(), &genesis).expect("Thread replica");
    (temp, repo, genesis, signer, replica)
}
fn capture(
    genesis: &ThreadGenesis,
    signer: &Ed25519Signer,
    parents: &[&SignedOperation],
    source_parents: Vec<StateId>,
) -> SignedOperation {
    let state = State::new_snapshot(Tree::new().hash(), source_parents, author());
    SignedOperation::sign(
        &ThreadOperation {
            version: 1,
            thread: genesis.id().expect("Thread ID"),
            parents: parents
                .iter()
                .map(|p| p.verify().expect("operation").id().expect("ID"))
                .collect(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("state")),
        },
        signer,
    )
    .expect("signed capture")
}
fn state_id(record: &SignedOperation) -> StateId {
    let ThreadOperationBody::Capture(bytes) = record.verify().expect("signed operation").body
    else {
        panic!("capture")
    };
    State::decode_current_msgpack(&bytes).expect("state").id()
}
fn discussion(
    genesis: &ThreadGenesis,
    signer: &Ed25519Signer,
    parents: &[&SignedOperation],
    key: &str,
) -> SignedOperation {
    let native_parents = parents
        .iter()
        .map(|record| {
            let ThreadOperationBody::Discussion(bytes) = record.verify().expect("signed").body
            else {
                panic!("discussion")
            };
            CollaborationOperationEnvelope::decode(&bytes)
                .expect("native op")
                .operation_id
        })
        .collect();
    let turn = DiscussionTurnV1::new(key).expect("turn");
    let body = if parents.is_empty() {
        CollaborationOperationBodyV1::Open {
            title: "Decision".into(),
            anchor: CollaborationAnchor::Repository,
            visibility: VisibilityTier::Public,
            turn,
            thread_ref: Some(genesis.id().expect("ID").to_hex()),
        }
    } else {
        CollaborationOperationBodyV1::AppendTurn { turn }
    };
    let native = CollaborationOperationEnvelope::new(
        "disc-018f47ea-4a54-7c89-b012-3456789abcde"
            .parse::<DiscussionRecordId>()
            .expect("discussion ID"),
        native_parents,
        CollaborationIdempotencyKey::new(key).expect("retry key"),
        author(),
        1,
        body,
    )
    .expect("native discussion");
    SignedOperation::sign(
        &ThreadOperation {
            version: 1,
            thread: genesis.id().expect("ID"),
            parents: parents
                .iter()
                .map(|p| p.verify().expect("op").id().expect("ID"))
                .collect(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Discussion(native.encode().expect("encode")),
        },
        signer,
    )
    .expect("signed discussion")
}

#[test]
fn offline_branches_and_discussion_converge_after_reordered_duplicate_delivery_and_restart() {
    let (_left_dir, left_repo, genesis, signer, left) = setup();
    let right_dir = TempDir::new().expect("right directory");
    let right_repo = Repository::init_default(right_dir.path()).expect("right repository");
    let right = ThreadReplica::open(right_repo.heddle_dir(), &genesis).expect("right replica");
    let a = capture(&genesis, &signer, &[], vec![genesis.base]);
    let b = capture(&genesis, &signer, &[], vec![genesis.base]);
    let d = discussion(&genesis, &signer, &[], "root");
    let da = discussion(&genesis, &signer, &[&d], "left contribution");
    let db = discussion(&genesis, &signer, &[&d], "right contribution");
    for record in [&a, &d, &da] {
        assert_eq!(
            left.receive(record, left_repo.store(), |_| Ok(()))
                .expect("local"),
            Admission::Accepted
        );
    }
    for record in [&b, &d, &db] {
        assert_eq!(
            right
                .receive(record, right_repo.store(), |_| Ok(()))
                .expect("local"),
            Admission::Accepted
        );
    }
    for record in [&db, &b, &db, &d] {
        left.receive(record, left_repo.store(), |_| Ok(()))
            .expect("replay");
    }
    for record in [&da, &a, &d, &a] {
        right
            .receive(record, right_repo.store(), |_| Ok(()))
            .expect("replay");
    }
    drop(left);
    drop(right);
    let left = ThreadReplica::open(left_repo.heddle_dir(), &genesis).expect("restart left");
    let right = ThreadReplica::open(right_repo.heddle_dir(), &genesis).expect("restart right");
    let lv = left.view().expect("left view");
    let rv = right.view().expect("right view");
    assert_eq!(lv.frontiers, rv.frontiers);
    assert_eq!(lv.collaboration, rv.collaboration);
    assert_eq!(
        lv.source_heads,
        BTreeSet::from([state_id(&a), state_id(&b)])
    );
    assert_eq!(
        lv.collaboration
            .discussions
            .values()
            .next()
            .expect("discussion")
            .turns
            .len(),
        3
    );
    let merged = capture(
        &genesis,
        &signer,
        &[&a, &b],
        vec![state_id(&a), state_id(&b)],
    );
    for (replica, store) in [(&left, left_repo.store()), (&right, right_repo.store())] {
        replica
            .receive(&merged, store, |_| Ok(()))
            .expect("integration");
        assert_eq!(
            replica.view().expect("view").source_heads,
            BTreeSet::from([state_id(&merged)])
        );
        assert!(
            store
                .get_state(&state_id(&merged))
                .expect("native store")
                .is_some()
        );
    }
}

#[test]
fn missing_parents_survive_restart_without_becoming_accepted() {
    let (_temp, repo, genesis, signer, replica) = setup();
    let parent = capture(&genesis, &signer, &[], vec![genesis.base]);
    let child = capture(&genesis, &signer, &[&parent], vec![state_id(&parent)]);
    assert_eq!(
        replica
            .receive(&child, repo.store(), |_| Ok(()))
            .expect("receive child"),
        Admission::Pending
    );
    let before = replica.generation().expect("generation");
    drop(replica);
    let replica = ThreadReplica::open(repo.heddle_dir(), &genesis).expect("restart");
    assert!(
        replica
            .view()
            .expect("pending view")
            .source_heads
            .is_empty()
    );
    assert!(
        repo.store()
            .get_state(&state_id(&child))
            .expect("native lookup")
            .is_none()
    );
    replica
        .receive(&parent, repo.store(), |_| Ok(()))
        .expect("parent arrives");
    assert!(replica.generation().expect("generation") > before);
    assert_eq!(
        replica.view().expect("admitted").source_heads,
        BTreeSet::from([state_id(&child)])
    );
}

#[test]
fn bad_signatures_and_denied_scope_never_persist_and_cross_facet_parents_are_rejected() {
    let (_temp, repo, genesis, signer, replica) = setup();
    let record = capture(&genesis, &signer, &[], vec![genesis.base]);
    let mut corrupt = record.clone();
    corrupt.signature[0] ^= 1;
    assert!(matches!(
        replica.receive(&corrupt, repo.store(), |_| Ok(())),
        Err(Error::SignedOperation(
            crypto::thread_operation::Error::Signature(_)
        ))
    ));
    assert!(
        matches!(replica.receive(&record,repo.store(), |_|Err(Error::Invalid("scope denied".into()))),Err(Error::Invalid(message)) if message=="scope denied")
    );
    assert_eq!(replica.generation().expect("no persistence"), 0);
    let d = discussion(&genesis, &signer, &[], "private");
    replica
        .receive(&d, repo.store(), |_| Ok(()))
        .expect("discussion");
    let cross = capture(&genesis, &signer, &[&d], vec![genesis.base]);
    assert!(
        matches!(replica.receive(&cross,repo.store(), |_|Ok(())).expect("reject graph"),Admission::Rejected(message) if message.contains("disclosure facet"))
    );
    assert!(replica.view().expect("view").source_heads.is_empty());
    let unseen = discussion(&genesis, &signer, &[], "unseen private root");
    let pending = discussion(&genesis, &signer, &[&unseen], "private descendant");
    assert_eq!(
        replica
            .receive(&pending, repo.store(), |_| Ok(()))
            .expect("pending discussion"),
        Admission::Pending
    );
    let cross_pending = capture(&genesis, &signer, &[&pending], vec![genesis.base]);
    assert!(
        matches!(replica.receive(&cross_pending, repo.store(), |_| Ok(())).expect("reject pending scope mismatch"), Admission::Rejected(message) if message.contains("disclosure facet"))
    );
}

#[test]
fn sharing_defaults_private_and_survives_reopen_with_revocation() {
    let (_temp, repo, genesis, _signer, replica) = setup();
    let destination = [9; 32];
    assert!(
        replica
            .sharing(&destination)
            .expect("private default")
            .0
            .is_empty()
    );
    let policy = replica
        .set_sharing(destination, &BTreeSet::from([ThreadFacet::Discussion]))
        .expect("opt in");
    drop(replica);
    let replica = ThreadReplica::open(repo.heddle_dir(), &genesis).expect("restart");
    assert_eq!(
        replica.sharing(&destination).expect("policy"),
        (BTreeSet::from([ThreadFacet::Discussion]), Some(policy))
    );
    replica
        .set_sharing(destination, &BTreeSet::new())
        .expect("revoke");
    assert!(replica.sharing(&destination).expect("revoked").0.is_empty());
}

#[test]
fn two_native_checkouts_capture_one_thread_without_rewriting_each_other() {
    use super::checkout::{CaptureInput, ThreadCheckout};
    let (temp, repo, genesis, signer, replica) = setup();
    let left = ThreadCheckout::create(
        &repo,
        &replica,
        &temp.path().join("left"),
        genesis.base,
        &crate::AudienceTier::Internal,
    )
    .expect("left checkout");
    let right = ThreadCheckout::create(
        &repo,
        &replica,
        &temp.path().join("right"),
        genesis.base,
        &crate::AudienceTier::Internal,
    )
    .expect("right checkout");
    let left_writer = left
        .claim_writer("agent-a".into(), Some(std::process::id()))
        .expect("left writer");
    let right_writer = right
        .claim_writer("agent-b".into(), Some(std::process::id()))
        .expect("right writer");
    assert!(
        left.claim_writer("agent-c".into(), Some(std::process::id()))
            .is_err()
    );
    std::fs::write(left.repository.root().join("work.txt"), "left work").expect("left edit");
    std::fs::write(right.repository.root().join("work.txt"), "right work").expect("right edit");
    let left_capture = left
        .capture(
            &replica,
            CaptureInput {
                lease: &left_writer.lease.lease_id,
                token: &left_writer.token,
                operation_id: "capture-a",
                expected: genesis.base,
                summary: "left work",
                attribution: author(),
            },
            &signer,
        )
        .expect("left native capture");
    assert_eq!(
        right.repository.head().expect("right HEAD"),
        Some(genesis.base)
    );
    let right_capture = right
        .capture(
            &replica,
            CaptureInput {
                lease: &right_writer.lease.lease_id,
                token: &right_writer.token,
                operation_id: "capture-b",
                expected: genesis.base,
                summary: "right work",
                attribution: author(),
            },
            &signer,
        )
        .expect("right native capture");
    assert_eq!(
        replica.view().expect("concurrent heads").source_heads,
        BTreeSet::from([state_id(&left_capture), state_id(&right_capture)])
    );
    let retry = left
        .capture(
            &replica,
            CaptureInput {
                lease: &left_writer.lease.lease_id,
                token: &left_writer.token,
                operation_id: "capture-a",
                expected: genesis.base,
                summary: "left work",
                attribution: author(),
            },
            &signer,
        )
        .expect("lost response retry");
    assert_eq!(left_capture, retry);
    // A stop after the durable receipt but before finalizing the active
    // journal must not leave the checkout unable to start another capture.
    let journal_path = left.repository.root().join(".heddle/capture-journal.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal_path).expect("capture journal"))
            .expect("journal JSON");
    journal["resulting"] = serde_json::Value::Null;
    std::fs::write(
        &journal_path,
        serde_json::to_vec(&journal).expect("journal bytes"),
    )
    .expect("simulate interrupted finalization");
    let recovered = left
        .capture(
            &replica,
            CaptureInput {
                lease: &left_writer.lease.lease_id,
                token: &left_writer.token,
                operation_id: "capture-a",
                expected: genesis.base,
                summary: "left work",
                attribution: author(),
            },
            &signer,
        )
        .expect("retry completed command after restart");
    assert_eq!(left_capture, recovered);
    let repaired: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal_path).expect("repaired journal"))
            .expect("repaired JSON");
    assert!(
        !repaired["resulting"].is_null(),
        "retry must finish the active journal"
    );
    assert_eq!(
        std::fs::read_to_string(left.repository.root().join("work.txt")).expect("left bytes"),
        "left work"
    );
    assert_eq!(
        std::fs::read_to_string(right.repository.root().join("work.txt")).expect("right bytes"),
        "right work"
    );
    std::fs::write(left.repository.root().join("work.txt"), "left next").expect("next left edit");
    std::fs::write(right.repository.root().join("work.txt"), "right next")
        .expect("next right edit");
    let start = std::sync::Barrier::new(2);
    let (left_next, right_next) = std::thread::scope(|scope| {
        let left_task = scope.spawn(|| {
            start.wait();
            left.capture(
                &replica,
                CaptureInput {
                    lease: &left_writer.lease.lease_id,
                    token: &left_writer.token,
                    operation_id: "capture-a-next",
                    expected: state_id(&left_capture),
                    summary: "left next",
                    attribution: author(),
                },
                &signer,
            )
            .expect("concurrent left capture")
        });
        let right_task = scope.spawn(|| {
            start.wait();
            right
                .capture(
                    &replica,
                    CaptureInput {
                        lease: &right_writer.lease.lease_id,
                        token: &right_writer.token,
                        operation_id: "capture-b-next",
                        expected: state_id(&right_capture),
                        summary: "right next",
                        attribution: author(),
                    },
                    &signer,
                )
                .expect("concurrent right capture")
        });
        (
            left_task.join().expect("left worker"),
            right_task.join().expect("right worker"),
        )
    });
    assert_eq!(
        replica
            .view()
            .expect("independent source heads")
            .source_heads,
        BTreeSet::from([state_id(&left_next), state_id(&right_next)])
    );
    let old_retry = left
        .capture(
            &replica,
            CaptureInput {
                lease: &left_writer.lease.lease_id,
                token: &left_writer.token,
                operation_id: "capture-a",
                expected: genesis.base,
                summary: "left work",
                attribution: author(),
            },
            &signer,
        )
        .expect("retry an older command after newer work");
    assert_eq!(old_retry, left_capture);
    assert_eq!(
        left.repository.head().expect("left HEAD"),
        Some(state_id(&left_next)),
        "retry must not rewind newer work"
    );
    assert_eq!(
        right.repository.head().expect("right HEAD"),
        Some(state_id(&right_next))
    );
}

#[test]
fn rejected_causal_parent_rejects_its_pending_descendants() {
    let (_temp, repo, genesis, signer, replica) = setup();
    let parent = capture(&genesis, &signer, &[], vec![]);
    let child = capture(&genesis, &signer, &[&parent], vec![state_id(&parent)]);
    let child_id = child.verify().expect("child").id().expect("ID");
    assert_eq!(
        replica
            .receive(&child, repo.store(), |_| Ok(()))
            .expect("pending child"),
        Admission::Pending
    );
    assert!(matches!(
        replica
            .receive(&parent, repo.store(), |_| Ok(()))
            .expect("invalid root"),
        Admission::Rejected(_)
    ));
    assert_eq!(
        replica
            .operation(&child_id)
            .expect("child status")
            .expect("stored child")
            .1,
        Admission::Rejected("causal parent was rejected".into())
    );
    assert!(replica.view().expect("view").pending.is_empty());
    assert!(
        repo.store()
            .get_state(&state_id(&child))
            .expect("state lookup")
            .is_none()
    );
}

#[test]
fn revision_lookup_requires_accepted_capture_in_the_selected_thread() {
    let (_dir, repo, genesis, signer, replica) = setup();
    let root = capture(&genesis, &signer, &[], vec![genesis.base]);
    let child = capture(&genesis, &signer, &[&root], vec![state_id(&root)]);
    let revision = state_id(&child);
    assert_eq!(
        replica
            .receive(&child, repo.store(), |_| Ok(()))
            .expect("pending capture"),
        Admission::Pending
    );
    assert!(
        replica
            .accepted_capture(revision)
            .expect("pending lookup")
            .is_none()
    );
    replica
        .receive(&root, repo.store(), |_| Ok(()))
        .expect("complete ancestry");
    assert_eq!(
        replica
            .accepted_capture(revision)
            .expect("accepted lookup")
            .expect("accepted capture")
            .id(),
        revision
    );
    let mut other_genesis = genesis.clone();
    other_genesis.nonce = vec![2];
    let other =
        ThreadReplica::open(repo.heddle_dir(), &other_genesis).expect("other Thread, same store");
    assert!(
        other
            .accepted_capture(revision)
            .expect("scoped lookup")
            .is_none(),
        "repository object presence is not Thread membership"
    );
    let invalid = capture(&genesis, &signer, &[], vec![revision]);
    assert!(matches!(
        replica
            .receive(&invalid, repo.store(), |_| Ok(()))
            .expect("rejected ancestry"),
        Admission::Rejected(_)
    ));
    assert!(
        replica
            .accepted_capture(state_id(&invalid))
            .expect("rejected lookup")
            .is_none()
    );
}

#[test]
fn peer_repair_does_not_treat_another_threads_operation_as_present() {
    let (_dir, repo, genesis, signer, replica) = setup();
    let root = capture(&genesis, &signer, &[], vec![genesis.base]);
    replica
        .receive(&root, repo.store(), |_| Ok(()))
        .expect("accepted capture");
    let operation_id = root.verify().expect("signature").id().expect("ID");
    let mut other_genesis = genesis;
    other_genesis.nonce = vec![2];
    let other = ThreadReplica::open(repo.heddle_dir(), &other_genesis).expect("other Thread");
    other
        .remember_peer_heads([8; 32], &[(ThreadFacet::Source, operation_id)])
        .expect("untrusted claim");
    assert_eq!(
        other
            .needed_from_peer([8; 32], &BTreeSet::from([ThreadFacet::Source]), 16)
            .expect("scope-bound repair"),
        vec![operation_id],
        "a record in another Thread cannot settle this peer's claim"
    );
}

#[test]
fn checkout_creation_rejects_a_revision_outside_its_thread_before_writing_files() {
    let (dir, repo, _genesis, _signer, replica) = setup();
    let unrelated = repo
        .snapshot(Some("another Thread".into()), None)
        .expect("local revision");
    let destination = dir.path().join("selected-checkout");
    let result = checkout::ThreadCheckout::create(
        &repo,
        &replica,
        &destination,
        unrelated.id(),
        &crate::AudienceTier::Internal,
    );
    assert!(
        result.is_err(),
        "materialization must require Thread membership"
    );
    assert!(
        !destination.exists(),
        "scope validation must precede filesystem writes"
    );
}
