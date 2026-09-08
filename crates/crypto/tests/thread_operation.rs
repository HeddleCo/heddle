use std::collections::BTreeSet;

use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, ContentHash, Principal, State, Tree,
    thread_replication::{ThreadOperation, ThreadOperationBody},
};

#[test]
fn portable_thread_signatures_bind_publisher_and_canonical_operation() {
    let signer = Ed25519Signer::from_seed(&[41; 32]).expect("fixed test key");
    let other = Ed25519Signer::from_seed(&[42; 32]).expect("different test key");
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![],
        Attribution::human(Principal::new("Author", "author@example.test")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: ContentHash::from_bytes([1; 32]),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("Ed25519 public key"),
        body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("state")),
    };
    let signed = SignedOperation::sign(&operation, &signer).expect("signed operation");
    assert_eq!(signed.verify().expect("portable verification"), operation);
    assert!(SignedOperation::sign(&operation, &other).is_err());

    let mut changed = operation.clone();
    changed.thread = ContentHash::from_bytes([2; 32]);
    assert!(
        SignedOperation {
            canonical: changed.encode().expect("canonical changed thread"),
            ..signed.clone()
        }
        .verify()
        .is_err()
    );
    changed = operation;
    changed.publisher = other.public_key().try_into().expect("other Ed25519 key");
    assert!(
        SignedOperation {
            canonical: changed.encode().expect("canonical changed publisher"),
            ..signed.clone()
        }
        .verify()
        .is_err()
    );
    let mut unsigned_domain = signed;
    unsigned_domain.signature = signer
        .sign(&unsigned_domain.canonical)
        .expect("wrong domain signature");
    assert!(unsigned_domain.verify().is_err());
}
