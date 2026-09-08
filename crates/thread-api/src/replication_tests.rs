// SPDX-License-Identifier: Apache-2.0
use std::collections::VecDeque;

use crypto::{Ed25519Signer, Signer};
use objects::object::{
    Attribution, Principal, State, Tree,
    thread_replication::{ThreadGenesis, ThreadOperation, ThreadOperationBody},
};
use repo::Repository;

use super::*;

#[test]
fn reconnect_finds_missing_ancestors_through_already_pending_parents() {
    let temp = tempfile::TempDir::new().expect("repository directory");
    let repository = Repository::init_default(temp.path()).expect("repository");
    let signer = Ed25519Signer::generate().expect("publisher");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "owned".into(),
        parent: None,
        base: repository.head().expect("HEAD").expect("initial state"),
        name: "offline".into(),
        intent: "repair interrupted causal delivery".into(),
        creator: signer.public_key().try_into().expect("Ed25519 key"),
        nonce: vec![],
    };
    let replica = ThreadReplica::open(repository.heddle_dir(), &genesis).expect("replica");
    let mut previous = None;
    let mut source_parent = genesis.base;
    let mut records = Vec::new();
    for _ in 0..3 {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![source_parent],
            Attribution::human(Principal::new("Agent", "agent@example.test")),
        );
        source_parent = state.id();
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: previous.into_iter().collect(),
            publisher: signer.public_key().try_into().expect("publisher key"),
            body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("state")),
        };
        previous = Some(operation.id().expect("operation ID"));
        records.push(SignedOperation::sign(&operation, &signer).expect("signed capture"));
    }
    for record in &records[1..] {
        assert_eq!(
            replica
                .receive(record, repository.store(), |_| Ok(()))
                .expect("pending"),
            Admission::Pending,
        );
    }
    let reopened = ThreadReplica::open(repository.heddle_dir(), &genesis).expect("restart");
    let mut session =
        Session::new(reopened, [2; 32], BTreeSet::from([ThreadFacet::Source]), 8).expect("session");
    let response = session
        .handle(
            Frame::Have(ReplicationHave {
                frontiers: vec![CausalFrontier {
                    facet: SharedFacet::Source as i32,
                    heads: vec![previous.expect("head").as_bytes().to_vec()],
                }],
            }),
            repository.store(),
        )
        .expect("repair request");
    let needed: Vec<_> = response
        .into_iter()
        .flat_map(|frame| match frame {
            Outbound::Frame(Frame::Need(need)) => need.operation_ids,
            _ => vec![],
        })
        .collect();
    assert_eq!(
        needed,
        vec![
            records[0]
                .verify()
                .expect("root")
                .id()
                .expect("root ID")
                .as_bytes()
                .to_vec()
        ]
    );
}

#[test]
fn wide_ancestry_resumes_through_a_small_window_and_retains_acceptance() {
    let left_dir = tempfile::TempDir::new().expect("left directory");
    let right_dir = tempfile::TempDir::new().expect("right directory");
    let left_repo = Repository::init_default(left_dir.path()).expect("left repository");
    let right_repo = Repository::init_default(right_dir.path()).expect("right repository");
    let signer = Ed25519Signer::generate().expect("publisher");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "owned".into(),
        parent: None,
        base: left_repo.head().expect("HEAD").expect("initial state"),
        name: "wide".into(),
        intent: "bounded causal repair".into(),
        creator: signer.public_key().try_into().expect("key"),
        nonce: vec![],
    };
    let left = ThreadReplica::open(left_repo.heddle_dir(), &genesis).expect("left replica");
    let right = ThreadReplica::open(right_repo.heddle_dir(), &genesis).expect("right replica");
    let facets = BTreeSet::from([ThreadFacet::Source]);
    left.set_sharing([2; 32], &facets)
        .expect("opt in to destination");
    let mut parents = BTreeSet::new();
    let mut source_parents = Vec::new();
    for _ in 0..20 {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![genesis.base],
            Attribution::human(Principal::new("Agent", "agent@example.test")),
        );
        source_parents.push(state.id());
        let operation = ThreadOperation {
            version: 1,
            thread: left.thread_id(),
            parents: BTreeSet::new(),
            publisher: genesis.creator,
            body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("state")),
        };
        parents.insert(operation.id().expect("ID"));
        let signed = SignedOperation::sign(&operation, &signer).expect("signed capture");
        left.receive(&signed, left_repo.store(), |_| Ok(()))
            .expect("local capture");
    }
    let merge = State::new_snapshot(
        Tree::new().hash(),
        source_parents,
        Attribution::human(Principal::new("Agent", "agent@example.test")),
    );
    let merge_operation = ThreadOperation {
        version: 1,
        thread: left.thread_id(),
        parents,
        publisher: genesis.creator,
        body: ThreadOperationBody::Capture(merge.encode_current_msgpack().expect("merge state")),
    };
    let merged_id = merge_operation.id().expect("merge ID");
    let signed = SignedOperation::sign(&merge_operation, &signer).expect("signed merge");
    left.receive(&signed, left_repo.store(), |_| Ok(()))
        .expect("local integration");
    let mut a = Session::new(left.clone(), [2; 32], facets.clone(), 2).expect("source session");
    let mut b =
        Session::new(right.clone(), [1; 32], facets.clone(), 2).expect("destination session");
    let frame = a.export_operation(merged_id).expect("integrated head");
    b.handle(frame, right_repo.store())
        .expect("receive head before parents");
    assert_eq!(
        right
            .operation(&merged_id)
            .expect("lookup")
            .expect("head")
            .1,
        Admission::Pending
    );
    // Lose the outstanding requests with the connection. The stored peer head
    // must refill the window without requiring any fresh mutation at the source.
    drop(b);
    let reopened = ThreadReplica::open(right_repo.heddle_dir(), &genesis).expect("restart");
    let mut b = Session::new(reopened, [1; 32], facets, 2).expect("resumed destination");
    let mut queue = VecDeque::from([(
        false,
        b.control().expect("repair").expect("needed ancestors"),
    )]);
    let mut delivered = 0;
    while let Some((from_a, frame)) = queue.pop_front() {
        delivered += 1;
        assert!(
            delivered < 500,
            "exchange must settle, not repeat receipts indefinitely"
        );
        let (receiver, store) = if from_a {
            (&mut b, right_repo.store())
        } else {
            (&mut a, left_repo.store())
        };
        let outgoing = receiver.handle(frame, store).expect("causal exchange");
        for item in outgoing {
            let frame = match item {
                Outbound::Frame(frame) => frame,
                Outbound::Operation(id) => receiver
                    .export_operation(id)
                    .expect("export under current policy"),
            };
            if let Frame::Need(need) = &frame {
                assert!(need.operation_ids.len() <= 2);
            }
            queue.push_back((!from_a, frame));
        }
    }
    assert_eq!(
        right.view().expect("destination view").source_heads,
        BTreeSet::from([merge.id()])
    );
    assert_eq!(
        left.peer_receipt([2; 32], merged_id)
            .expect("durable receipt"),
        Some(Admission::Accepted)
    );
    left.record_peer_receipt([2; 32], merged_id, &Admission::Pending)
        .expect("out-of-order receipt");
    let reopened = ThreadReplica::open(left_repo.heddle_dir(), &genesis).expect("sender restart");
    assert_eq!(
        reopened
            .peer_receipt([2; 32], merged_id)
            .expect("retained receipt"),
        Some(Admission::Accepted)
    );
    let queued = a
        .handle(
            Frame::Need(ReplicationNeed {
                operation_ids: vec![merged_id.as_bytes().to_vec()],
            }),
            left_repo.store(),
        )
        .expect("queue an export");
    assert!(matches!(queued.first(), Some(Outbound::Operation(_))));
    left.set_sharing([2; 32], &BTreeSet::new())
        .expect("revoke sharing before writer sends");
    assert!(matches!(
        a.export_operation(merged_id),
        Err(Error::Protocol(
            "operation is outside current sharing policy"
        ))
    ));
}

#[test]
fn paged_announcement_restarts_when_a_write_lands_behind_its_cursor() {
    let dir = tempfile::TempDir::new().expect("repository directory");
    let repository = Repository::init_default(dir.path()).expect("repository");
    let signer = Ed25519Signer::generate().expect("publisher");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "owned".into(),
        parent: None,
        base: repository.head().expect("HEAD").expect("base"),
        name: "paged".into(),
        intent: "no changefeed handoff gap".into(),
        creator: signer.public_key().try_into().expect("key"),
        nonce: vec![],
    };
    let replica = ThreadReplica::open(repository.heddle_dir(), &genesis).expect("replica");
    let mut records = Vec::new();
    for _ in 0..2 {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![genesis.base],
            Attribution::human(Principal::new("Agent", "agent@example.test")),
        );
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: BTreeSet::new(),
            publisher: genesis.creator,
            body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("state")),
        };
        records.push((
            operation.id().expect("ID"),
            SignedOperation::sign(&operation, &signer).expect("signed capture"),
        ));
    }
    records.sort_by_key(|(id, _)| *id);
    let [(earlier_id, earlier), (_, later)] = records.as_slice() else {
        panic!("two records");
    };
    replica
        .receive(later, repository.store(), |_| Ok(()))
        .expect("first write");
    let facets = BTreeSet::from([ThreadFacet::Source]);
    replica.set_sharing([2; 32], &facets).expect("opt-in");
    let mut session = Session::new(replica.clone(), [2; 32], facets, 1).expect("session");
    assert!(matches!(
        session.announcement().expect("first page"),
        Some(Frame::Have(_))
    ));
    replica
        .receive(earlier, repository.store(), |_| Ok(()))
        .expect("concurrent write sorts behind consumed cursor");
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    while let Some(frame) = session.announcement().expect("continue paged announcement") {
        pages += 1;
        assert!(pages < 10, "announcement must settle");
        if let Frame::Have(have) = frame {
            seen.extend(have.frontiers.into_iter().flat_map(|f| f.heads));
        }
    }
    assert!(
        seen.contains(earlier_id.as_bytes().as_slice()),
        "None must mean the current generation was fully announced"
    );
}
