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

use super::{Error, ThreadReplica};

#[test]
fn bare_authority_receipt_is_not_an_evergreen_admission_after_restart() {
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
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(
            author.public_key().try_into().expect("key"),
        ),
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
            version: 3,
            basis: objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,
            spool,
            spool_genesis,
            thread: operation.thread,
            subject:
                objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(
                    operation.id().expect("ID"),
                ),
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
    parent_receipt
        .verify(
            &parent,
            &objects::object::thread_replication::integration::TrustedHostedExecutor {
                spool,
                spool_genesis,
                executor: executor.public_key().try_into().expect("key"),
            },
        )
        .expect("signature/binding control");
    assert!(matches!(
        replica.receive_with_authority_admission(
            &child,
            &child_receipt,
            repository.store(),
            |_| Ok(())
        ),
        Err(Error::WitnessEvidenceRequired)
    ));
    replica
        .connect()
        .expect("db")
        .execute(
            "INSERT INTO hosted_executor_pins VALUES(?1,?2,?3)",
            rusqlite::params![
                spool.to_string(),
                spool_genesis.as_bytes(),
                executor.public_key()
            ],
        )
        .expect("existing evergreen key");
    assert!(matches!(
        replica.receive_with_authority_admission(
            &child,
            &child_receipt,
            repository.store(),
            |_| Ok(())
        ),
        Err(Error::WitnessEvidenceRequired)
    ));
    assert!(replica.operation(&child_id).expect("lookup").is_none());
    let reopened =
        ThreadReplica::open(repository.heddle_dir(), replica.thread_id()).expect("restart");
    assert!(matches!(
        reopened.receive_with_authority_admission(
            &parent,
            &parent_receipt,
            repository.store(),
            |_| Ok(())
        ),
        Err(Error::WitnessEvidenceRequired)
    ));
    assert!(
        !replica
            .original_authority_admitted(&child)
            .expect("no authority marker")
    );
}
