use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer,
    thread_authority_admission::SignedAuthorityAdmission,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::object::{
    CollaborationActor, ContentHash,
    thread_authority_admission::ThreadAuthorityAdmission,
    thread_replication::{
        ThreadGenesis, ThreadOperation, ThreadOperationBody,
        integration::SPOOL_GENESIS_TRUST_FORMAT,
        metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
    },
};
use prost::Message;
use uuid::Uuid;

use super::{Admission, Error, ThreadReplica};

#[test]
fn retained_receipt_is_atomic_with_pending_bytes_and_survives_restart_without_original_credential()
{
    let directory = tempfile::tempdir().expect("repository");
    let repository = crate::Repository::init_default(directory.path()).expect("repository");
    let author = Ed25519Signer::from_seed(&[61; 32]).expect("foreign original author");
    let executor = Ed25519Signer::from_seed(&[62; 32]).expect("selected hosted executor");
    let owner = Ed25519Signer::from_seed(&[63; 32]).expect("Spool owner");
    let spool = Uuid::from_u128(1064);
    let genesis = ThreadGenesis {
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base: repository.head().expect("head").expect("base"),
        name: "shared".into(),
        intent: "retain exact foreign original author".into(),
        creator: author.public_key().try_into().expect("key"),
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(author.public_key().try_into().expect("key")),
        nonce: vec![65; 32],
    };
    let signed_genesis = SignedGenesis::sign(&genesis, &author).expect("creator proof");
    let replica = ThreadReplica::create(repository.heddle_dir(), &signed_genesis).expect("Thread");
    let owner_genesis =
        crate::sign_spool_owner_genesis(&owner, *spool.as_bytes()).expect("selected owner genesis");
    let spool_genesis = ContentHash::compute_typed(
        SPOOL_GENESIS_TRUST_FORMAT,
        &owner_genesis
            .genesis
            .as_ref()
            .expect("genesis")
            .encode_to_vec(),
    );
    let make = |name: &str, parents, nonce| {
        let envelope = b"first author permission was checked by enrolled executor; no live credential retained here".to_vec();
        let control = ThreadControl {
            version: 1,
            spool,
            actor: CollaborationActor {
                principal_id: Uuid::from_u128(2064),
                agent_id: Some("foreign-agent".into()),
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
            authority_envelope: envelope,
            client_operation_id: Uuid::from_u128(nonce),
            occurred_at_ms: 1,
            control: Control::Name(name.into()),
        };
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents,
            publisher: author.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Metadata(control.encode().expect("control")),
        };
        let receipt = ThreadAuthorityAdmission {
            version: 1,
            spool,
            spool_genesis,
            thread: operation.thread,
            operation: operation.id().expect("ID"),
            actor: control.actor,
            publisher: operation.publisher,
            authority_digest: control.authority_digest,
            executor: executor.public_key().try_into().expect("key"),
            admitted_at_ms: 2000,
        };
        (
            SignedOperation::sign(&operation, &author).expect("original signature"),
            SignedAuthorityAdmission::sign(&receipt, &executor).expect("host testimony"),
        )
    };
    let (parent, parent_receipt) = make("parent", BTreeSet::new(), 3064);
    let parent_id = parent.verify().expect("parent").id().expect("ID");
    let (child, child_receipt) = make("child", BTreeSet::from([parent_id]), 3065);
    let child_id = child.verify().expect("child").id().expect("ID");
    assert!(
        replica
            .receive_with_authority_admission(
                &child,
                &child_receipt,
                repository.store(),
                |_| Ok(())
            )
            .expect_err("incoming testimony cannot enroll its executor")
            .to_string()
            .contains("independently pinned executor")
    );
    assert!(replica.operation(&child_id).expect("lookup").is_none());
    repository
        .verify_and_pin_owner_genesis(
            2,
            Some(&owner_genesis),
            &["selected".into(), "shared".into()],
        )
        .expect("independent selected owner enrollment");
    repository
        .pin_thread_hosted_executor(&replica, executor.public_key().try_into().expect("key"))
        .expect("independent selected endpoint enrollment");
    assert!(
        replica
            .receive_with_authority_admission(&child, &child_receipt, repository.store(), |_| Err(
                Error::Invalid("current delivery denied".into())
            ))
            .expect_err("historical testimony never authorizes current delivery")
            .to_string()
            .contains("current delivery denied")
    );
    assert!(replica.operation(&child_id).expect("lookup").is_none());

    // Fail after immutable bytes, authority marker and receipt have been written,
    // at the next parent insert in that same transaction. Nothing may survive.
    replica.connect().expect("connection").execute_batch("CREATE TRIGGER test_receipt_rollback BEFORE INSERT ON parents BEGIN SELECT RAISE(ABORT,'forced receipt transaction rollback'); END;").expect("rollback seam");
    assert!(
        replica
            .receive_with_authority_admission(
                &child,
                &child_receipt,
                repository.store(),
                |_| Ok(())
            )
            .expect_err("transaction rollback")
            .to_string()
            .contains("forced receipt transaction rollback")
    );
    assert!(
        replica
            .operation(&child_id)
            .expect("rollback lookup")
            .is_none(),
        "no original bytes or attached receipt survive aborted transaction"
    );
    assert!(
        !replica
            .original_authority_admitted(&child)
            .expect("no authority marker")
    );
    replica
        .connect()
        .expect("connection")
        .execute_batch("DROP TRIGGER test_receipt_rollback;")
        .expect("remove rollback seam");
    assert_eq!(
        replica
            .receive_with_authority_admission(
                &child,
                &child_receipt,
                repository.store(),
                |_| Ok(())
            )
            .expect("host-admitted pending child"),
        Admission::Pending
    );
    let reopened =
        ThreadReplica::open(repository.heddle_dir(), replica.thread_id()).expect("restart");
    let stored = reopened
        .operation_with_authority_admission(&child_id)
        .expect("lookup")
        .expect("pending original");
    assert_eq!(stored.original, child);
    assert_eq!(stored.status, Admission::Pending);
    assert_eq!(
        stored.authority_admission,
        Some(child_receipt.clone()),
        "pending original retains exact authority receipt"
    );
    assert!(
        reopened
            .original_authority_admitted(&child)
            .expect("durable original admission")
    );
    let mut later = child_receipt.verify_signature().expect("receipt");
    later.admitted_at_ms += 1000;
    let later_receipt =
        SignedAuthorityAdmission::sign(&later, &executor).expect("another valid receipt");
    assert_eq!(
        reopened
            .receive_with_authority_admission(
                &child,
                &later_receipt,
                repository.store(),
                |_| Ok(())
            )
            .expect("replay"),
        Admission::Pending
    );
    assert_eq!(
        reopened
            .operation_with_authority_admission(&child_id)
            .expect("lookup")
            .expect("original")
            .authority_admission,
        Some(child_receipt.clone()),
        "later courier cannot replace first retained testimony"
    );

    // No original author Biscuit, live owner association or execution timestamp
    // is reconstituted while accepted causal parents arrive after a restart.
    assert_eq!(
        reopened
            .receive_with_authority_admission(&parent, &parent_receipt, repository.store(), |_| Ok(
                ()
            ))
            .expect("verified parent"),
        Admission::Accepted
    );
    let accepted = reopened
        .operation_with_authority_admission(&child_id)
        .expect("lookup")
        .expect("child");
    assert_eq!(accepted.status, Admission::Accepted);
    assert_eq!(accepted.authority_admission, Some(child_receipt));
    assert_eq!(
        reopened
            .metadata_frontier(&objects::object::thread_replication::metadata::Property::Name)
            .expect("field frontier")[0]
            .0,
        child_id
    );
    assert_eq!(
        repository.head().expect("checkout head"),
        Some(genesis.base),
        "authority receipt never mutates checkout files"
    );
}
