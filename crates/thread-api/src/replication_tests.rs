// SPDX-License-Identifier: Apache-2.0
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
            replica.receive(record, repository.store(), |_| Ok(())).expect("pending"),
            Admission::Pending,
        );
    }
    let reopened = ThreadReplica::open(repository.heddle_dir(), &genesis).expect("restart");
    let mut session = Session::new(reopened, [2; 32], BTreeSet::from([ThreadFacet::Source]), 8)
        .expect("session");
    let response = session.handle(Frame::Have(ReplicationHave {
        frontiers: vec![CausalFrontier {
            facet: SharedFacet::Source as i32,
            heads: vec![previous.expect("head").as_bytes().to_vec()],
        }],
    }), repository.store()).expect("repair request");
    let needed: Vec<_> = response.into_iter().flat_map(|frame| match frame {
        Outbound::Frame(Frame::Need(need)) => need.operation_ids,
        _ => vec![],
    }).collect();
    assert_eq!(needed, vec![records[0].verify().expect("root").id().expect("root ID").as_bytes().to_vec()]);
}
