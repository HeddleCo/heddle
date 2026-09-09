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
fn local_creation_retains_creator_proof_and_reopens_by_id_without_the_key() {
    use crypto::thread_operation::SignedGenesis;
    let (_dir, repo, mut genesis, signer, _replica) = setup();
    genesis.nonce.push(93);
    let signed = SignedGenesis::sign(&genesis, &signer).expect("original proof");
    let created = ThreadReplica::create(repo.heddle_dir(), &signed).expect("signed creation");
    let id = created.thread_id();
    drop(created);
    drop(signer);
    let reopened = ThreadReplica::open(repo.heddle_dir(), id).expect("reopen by stable ID");
    assert_eq!(
        reopened.signed_genesis().expect("durable original proof"),
        signed
    );
    let relay = TempDir::new().expect("another device");
    let copied = ThreadReplica::create(
        relay.path(),
        &reopened.signed_genesis().expect("relay proof"),
    )
    .expect("no creator key needed");
    assert_eq!(copied.thread_id(), id);
    assert_eq!(copied.signed_genesis().expect("unchanged proof"), signed);
    let missing = TempDir::new().expect("empty device");
    assert!(ThreadReplica::open(missing.path(), id).is_err());
    assert!(
        !missing
            .path()
            .join(crate::local_metadata::DATABASE_NAME)
            .exists(),
        "a lookup cannot create storage"
    );
    let mut invalid = signed;
    invalid.signature[0] ^= 1;
    assert!(ThreadReplica::create(missing.path(), &invalid).is_err());
    assert!(
        !missing
            .path()
            .join(crate::local_metadata::DATABASE_NAME)
            .exists(),
        "invalid proof must fail before storage creation"
    );
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
    assert!(
        ThreadReplica::create(
            repository.heddle_dir(),
            &crypto::thread_operation::SignedGenesis {
                canonical: vec![0; objects::object::thread_replication::MAX_OPERATION_BYTES + 1],
                signature: vec![0; 64]
            }
        )
        .is_err()
    );
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
    let replica = ThreadReplica::create(
        repo.heddle_dir(),
        &crypto::thread_operation::SignedGenesis::sign(&genesis, &signer).expect("creator proof"),
    )
    .expect("Thread replica");
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
            body: ThreadOperationBody::Capture(
                state.encode_current_msgpack().expect("state").into(),
            ),
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
    State::decode_current_msgpack(&bytes.state)
        .expect("state")
        .id()
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
            blocking: false,
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
    let right = ThreadReplica::create(
        right_repo.heddle_dir(),
        &crypto::thread_operation::SignedGenesis::sign(&genesis, &signer).expect("creator proof"),
    )
    .expect("right replica");
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
    let left = ThreadReplica::open(left_repo.heddle_dir(), genesis.id().expect("Thread ID"))
        .expect("restart left");
    let right = ThreadReplica::open(right_repo.heddle_dir(), genesis.id().expect("Thread ID"))
        .expect("restart right");
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
    let replica =
        ThreadReplica::open(repo.heddle_dir(), genesis.id().expect("Thread ID")).expect("restart");
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
    let replica =
        ThreadReplica::open(repo.heddle_dir(), genesis.id().expect("Thread ID")).expect("restart");
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
fn revision_lookup_requires_accepted_source_revision_in_the_selected_thread() {
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
            .accepted_source_revision(revision)
            .expect("pending lookup")
            .is_none()
    );
    replica
        .receive(&root, repo.store(), |_| Ok(()))
        .expect("complete ancestry");
    assert_eq!(
        replica
            .accepted_source_revision(revision)
            .expect("accepted lookup")
            .expect("accepted capture")
            .id(),
        revision
    );
    let mut other_genesis = genesis.clone();
    other_genesis.nonce = vec![2];
    let other = ThreadReplica::create(
        repo.heddle_dir(),
        &crypto::thread_operation::SignedGenesis::sign(&other_genesis, &signer)
            .expect("creator proof"),
    )
    .expect("other Thread, same store");
    assert!(
        other
            .accepted_source_revision(revision)
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
            .accepted_source_revision(state_id(&invalid))
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
    let other = ThreadReplica::create(
        repo.heddle_dir(),
        &crypto::thread_operation::SignedGenesis::sign(&other_genesis, &signer)
            .expect("creator proof"),
    )
    .expect("other Thread");
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

#[test]
fn hosted_integration_requires_independent_persistent_executor_trust_and_never_writes_checkout() {
    use objects::object::thread_replication::integration::{
        HostedIntegration, SPOOL_GENESIS_TRUST_FORMAT,
    };
    use prost::Message;
    let (_dir, repo, genesis, signer, replica) = setup();
    let head_before = repo.head().expect("checkout HEAD");
    let parent = capture(&genesis, &signer, &[], vec![genesis.base]);
    replica
        .receive(&parent, repo.store(), |_| Ok(()))
        .expect("target source");
    let owner = Ed25519Signer::from_seed(&[22; 32]).expect("owner");
    let executor = Ed25519Signer::from_seed(&[23; 32]).expect("executor");
    let spool: uuid::Uuid = genesis.spool.parse().expect("Spool");
    let owner_genesis =
        crate::sign_spool_owner_genesis(&owner, *spool.as_bytes()).expect("signed owner genesis");
    let result = State::new_snapshot(
        Tree::new().hash(),
        vec![state_id(&parent)],
        Attribution::human(Principal::new("source", "source@example.test")),
    );
    let receipt = HostedIntegration {
        version: 1,
        spool,
        spool_genesis: ContentHash::compute_typed(
            SPOOL_GENESIS_TRUST_FORMAT,
            &owner_genesis
                .genesis
                .as_ref()
                .expect("genesis body")
                .encode_to_vec(),
        ),
        executor: executor.public_key().try_into().expect("endpoint"),
        source_thread: ContentHash::from_bytes([24; 32]),
        source_operation: ContentHash::from_bytes([25; 32]),
        source_revision: result.id(),
        target_thread: replica.thread_id(),
        expected_target_frontier: BTreeSet::from([parent
            .verify()
            .expect("parent")
            .id()
            .expect("ID")]),
        result: result.encode_current_msgpack().expect("result"),
        initiating_request_proof: ContentHash::from_bytes([26; 32]),
        review_policy_version: ContentHash::from_bytes([27; 32]),
        review_evidence: BTreeSet::new(),
        executed_at_ms: 100,
    };
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: receipt.expected_target_frontier.clone(),
        publisher: receipt.executor,
        body: ThreadOperationBody::Integration(receipt.encode().expect("receipt")),
    };
    let signed = SignedOperation::sign(&operation, &executor).expect("executor attestation");
    for _ in 0..2 {
        let error = replica
            .receive(&signed, repo.store(), |_| Ok(()))
            .expect_err("an ordinary delivery grant never establishes executor trust");
        assert!(
            error
                .to_string()
                .contains("independently pinned executor trust")
        );
        assert!(
            replica
                .operation(&operation.id().expect("ID"))
                .expect("lookup")
                .is_none()
        );
        assert_eq!(
            replica.view().expect("view").source_heads,
            BTreeSet::from([state_id(&parent)])
        );
        assert_eq!(repo.head().expect("unchanged checkout"), head_before);
    }
    assert!(
        repo.pin_thread_hosted_executor(&replica, receipt.executor)
            .is_err(),
        "incoming attestation cannot supply missing owner pin"
    );
    repo.verify_and_pin_owner_genesis(
        2,
        Some(&owner_genesis),
        &["selected".into(), "spool".into()],
    )
    .expect("selected remote owner genesis");
    repo.pin_thread_hosted_executor(&replica, receipt.executor)
        .expect("independent selected endpoint");
    repo.pin_thread_hosted_executor(&replica, receipt.executor)
        .expect("idempotent pin");
    assert!(
        matches!(replica.receive(&signed,repo.store(), |_|Err(Error::Invalid("active delivery denied".into()))),Err(Error::Invalid(reason)) if reason=="active delivery denied"),
        "historical endpoint trust does not grant active writes"
    );
    assert_eq!(
        replica
            .receive(&signed, repo.store(), |_| Ok(()))
            .expect("trusted receipt"),
        Admission::Accepted
    );
    let reopened = ThreadReplica::open(repo.heddle_dir(), replica.thread_id()).expect("reopen");
    assert_eq!(
        reopened
            .receive(&signed, repo.store(), |_| Ok(()))
            .expect("persistent trust and original replay"),
        Admission::Accepted
    );
    assert_eq!(
        reopened.view().expect("source projection").source_heads,
        BTreeSet::from([result.id()])
    );
    assert_eq!(
        reopened
            .accepted_source_revision(result.id())
            .expect("membership")
            .expect("accepted result")
            .id(),
        result.id()
    );
    assert_eq!(
        repo.head().expect("integration is metadata only"),
        head_before
    );
    let after = capture(&genesis, &signer, &[&signed], vec![result.id()]);
    reopened
        .receive(&after, repo.store(), |_| Ok(()))
        .expect("later native capture");
    assert_eq!(
        reopened.view().expect("next source head").source_heads,
        BTreeSet::from([state_id(&after)])
    );
    assert_eq!(
        repo.head().expect("receive never checks out remote state"),
        head_before
    );
    let mut foreign_receipt = receipt.clone();
    foreign_receipt.spool_genesis = ContentHash::from_bytes([99; 32]);
    let mut foreign = operation.clone();
    foreign.body =
        ThreadOperationBody::Integration(foreign_receipt.encode().expect("foreign receipt"));
    let foreign =
        SignedOperation::sign(&foreign, &executor).expect("valid signature is insufficient");
    assert!(
        reopened
            .receive(&foreign, repo.store(), |_| Ok(()))
            .is_err(),
        "executor cannot swap immutable genesis"
    );
    let other = Ed25519Signer::from_seed(&[24; 32]).expect("unselected endpoint");
    foreign_receipt = receipt;
    foreign_receipt.executor = other.public_key().try_into().expect("endpoint");
    let mut foreign = operation;
    foreign.publisher = foreign_receipt.executor;
    foreign.body = ThreadOperationBody::Integration(foreign_receipt.encode().expect("receipt"));
    assert!(
        reopened
            .receive(
                &SignedOperation::sign(&foreign, &other).expect("other valid signer"),
                repo.store(),
                |_| Ok(())
            )
            .is_err(),
        "self-signed executor never enrolls itself"
    );
}

#[test]
fn selected_native_capture_keeps_unselected_work_out_of_the_snapshot() {
    use super::checkout::{CaptureInput, ThreadCheckout};
    let (temp, repo, genesis, signer, replica) = setup();
    let checkout = ThreadCheckout::create(
        &repo,
        &replica,
        &temp.path().join("selected"),
        genesis.base,
        &crate::AudienceTier::Internal,
    )
    .expect("checkout");
    let writer = checkout
        .claim_writer("writer".into(), None)
        .expect("writer");
    std::fs::write(checkout.repository.root().join("selected.txt"), "selected").expect("edit");
    std::fs::write(checkout.repository.root().join("other.txt"), "keep working").expect("edit");
    let input = CaptureInput {
        lease: &writer.lease.lease_id,
        token: &writer.token,
        operation_id: "selected",
        expected: genesis.base,
        summary: "selection",
        attribution: author(),
    };
    let signed = checkout
        .capture_with_paths(&replica, input.clone(), &signer, &["selected.txt".into()])
        .expect("capture selection");
    let captured = signed
        .verify()
        .expect("proof")
        .source_state()
        .expect("source")
        .expect("State");
    let tree = repo
        .store()
        .get_tree(&captured.tree)
        .expect("tree")
        .expect("stored tree");
    assert!(
        tree.entries()
            .iter()
            .any(|entry| entry.name() == "selected.txt")
    );
    assert!(
        !tree
            .entries()
            .iter()
            .any(|entry| entry.name() == "other.txt"),
        "unselected working file must not enter source snapshot"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.repository.root().join("other.txt"))
            .expect("working file"),
        "keep working"
    );
    assert_eq!(
        checkout
            .capture_with_paths(&replica, input.clone(), &signer, &["selected.txt".into()])
            .expect("exact retry"),
        signed
    );
    assert!(
        checkout
            .capture_with_paths(&replica, input, &signer, &["other.txt".into()])
            .is_err(),
        "retry cannot change selected paths"
    );
}

#[test]
fn local_integration_requires_original_source_frontier_cas_and_preserves_private_audience() {
    use objects::object::{
        StateVisibility, thread_replication::local_integration::LocalIntegration,
    };
    let (_directory, repository, source_genesis, signer, source) = setup();
    let original = capture(&source_genesis, &signer, &[], vec![source_genesis.base]);
    source
        .receive(&original, repository.store(), |_| Ok(()))
        .expect("source admission");
    let source_state = state_id(&original);
    let private = VisibilityTier::Private {
        scope_label: "owner".into(),
    };
    repository
        .put_state_visibility_if_absent(StateVisibility {
            state: source_state,
            tier: private.clone(),
            embargo_until: None,
            declarer: author().principal,
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("source privacy");
    let mut target_genesis = source_genesis.clone();
    target_genesis.name = "target".into();
    target_genesis.nonce = vec![8];
    let target = ThreadReplica::create(
        repository.heddle_dir(),
        &crypto::thread_operation::SignedGenesis::sign(&target_genesis, &signer)
            .expect("target proof"),
    )
    .expect("target");
    let result = State::new_merge(
        Tree::new().hash(),
        vec![target_genesis.base, source_state],
        author(),
    );
    let receipt = LocalIntegration {
        version: 1,
        spool: uuid::Uuid::parse_str(&source_genesis.spool).expect("spool"),
        device: signer.public_key().try_into().expect("key"),
        source_thread: source.thread_id(),
        source_operation: original.verify().expect("source").id().expect("source ID"),
        source_revision: source_state,
        target_thread: target.thread_id(),
        expected_target_frontier: BTreeSet::new(),
        result: result.encode_current_msgpack().expect("state"),
        result_visibility: private.clone(),
        initiating_request_proof: ContentHash::from_bytes([3; 32]),
        local_policy_version: ContentHash::from_bytes([4; 32]),
        executed_at_ms: 100,
    };
    let make = |receipt: LocalIntegration| {
        SignedOperation::sign(
            &ThreadOperation {
                version: 1,
                thread: target.thread_id(),
                parents: receipt.expected_target_frontier.clone(),
                publisher: receipt.device,
                body: ThreadOperationBody::LocalIntegration(receipt.encode().expect("receipt")),
            },
            &signer,
        )
        .expect("operation")
    };
    let mut unrelated = receipt.clone();
    unrelated.source_operation = ContentHash::from_bytes([99; 32]);
    assert!(
        target
            .receive_local_integration_cas(&make(unrelated), repository.store(), |_| Ok(()))
            .expect_err("original source required")
            .to_string()
            .contains("original source")
    );
    let mut public = receipt.clone();
    public.result_visibility = VisibilityTier::Public;
    assert!(
        target
            .receive_local_integration_cas(&make(public), repository.store(), |_| Ok(()))
            .expect_err("no audience downgrade")
            .to_string()
            .contains("weakens source audience")
    );
    let signed = make(receipt.clone());
    assert_eq!(
        target
            .receive_local_integration_cas(&signed, repository.store(), |_| Ok(()))
            .expect("landing"),
        Admission::Accepted
    );
    assert_eq!(
        repository
            .effective_visibility_tier(&result.id())
            .expect("result privacy"),
        private
    );
    assert_eq!(
        target
            .receive_local_integration_cas(&signed, repository.store(), |_| Ok(()))
            .expect("exact retry"),
        Admission::Accepted
    );
    let mut stale = receipt;
    stale.executed_at_ms += 1;
    assert!(
        target
            .receive_local_integration_cas(&make(stale.clone()), repository.store(), |_| Ok(()))
            .expect_err("CAS")
            .to_string()
            .contains("frontier changed")
    );
    assert_eq!(
        target
            .receive(&make(stale), repository.store(), |_| Ok(()))
            .expect("historical concurrent landing"),
        Admission::Accepted,
        "replication retains authenticated historic branches; only fresh execution compares current frontier"
    );
    assert_eq!(
        repository.head().expect("original checkout"),
        Some(source_genesis.base)
    );
}

#[test]
fn explicit_source_resolution_requires_current_candidates_and_retains_all_parents() {
    use super::checkout::ThreadCheckout;
    let (temp, repository, genesis, signer, replica) = setup();
    let left = capture(&genesis, &signer, &[], vec![genesis.base]);
    let right = capture(&genesis, &signer, &[], vec![genesis.base]);
    assert_ne!(state_id(&left), state_id(&right));
    replica
        .receive(&left, repository.store(), |_| Ok(()))
        .expect("left");
    replica
        .receive(&right, repository.store(), |_| Ok(()))
        .expect("right");
    let checkout = ThreadCheckout::create(
        &repository,
        &replica,
        &temp.path().join("resolution"),
        genesis.base,
        &crate::AudienceTier::Internal,
    )
    .expect("checkout");
    let writer = checkout
        .claim_writer("resolver".into(), None)
        .expect("writer");
    let heads = replica.view().expect("heads").source_heads;
    let version = source_conflict_version(replica.thread_id(), &heads);
    let operation = uuid::Uuid::new_v4().to_string();
    assert!(
        checkout
            .resolve_source_choice(
                &replica,
                &writer.lease.lease_id,
                &writer.token,
                &operation,
                genesis.base,
                &[0; 32],
                state_id(&left),
                author(),
                &signer
            )
            .expect_err("stale conflict set")
            .to_string()
            .contains("candidates changed")
    );
    let resolved = checkout
        .resolve_source_choice(
            &replica,
            &writer.lease.lease_id,
            &writer.token,
            &operation,
            genesis.base,
            &version,
            state_id(&left),
            author(),
            &signer,
        )
        .expect("explicit selection");
    let state = resolved
        .verify()
        .expect("proof")
        .source_state()
        .expect("source")
        .expect("State");
    assert_eq!(
        state.parents.iter().copied().collect::<BTreeSet<_>>(),
        heads
    );
    assert_eq!(
        replica.view().expect("resolved").source_heads,
        BTreeSet::from([state.id()])
    );
    assert_eq!(
        checkout
            .resolve_source_choice(
                &replica,
                &writer.lease.lease_id,
                &writer.token,
                &operation,
                genesis.base,
                &version,
                state_id(&left),
                author(),
                &signer
            )
            .expect("retry"),
        resolved
    );
}
