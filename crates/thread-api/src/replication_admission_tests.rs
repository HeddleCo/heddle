use std::{collections::BTreeSet, sync::Arc};

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
        metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
    },
};
use repo::{Repository, thread_replication::ThreadReplica};
use uuid::Uuid;

use super::{native::LocalReplica, *};

#[tokio::test]
async fn structural_authority_sidecars_never_authorize_native_receive() {
    use crate::replication::store::{ReceivedOperation, ReplicaStore};
    let dir = tempfile::tempdir().expect("receiver");
    let repository = Repository::init_default(dir.path()).expect("repository");
    let author = Ed25519Signer::from_seed(&[81; 32]).expect("original publisher");
    let executor = Ed25519Signer::from_seed(&[82; 32]).expect("receipt signer");
    let spool = Uuid::from_u128(4084);
    let genesis = ThreadGenesis {
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base: repository.head().expect("head").expect("base"),
        name: "structural evidence".into(),
        intent: String::new(),
        creator: author.public_key().try_into().expect("key"),
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(
            author.public_key().try_into().expect("key"),
        ),
        nonce: vec![85; 32],
    };
    let replica = ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&genesis, &author).expect("genesis"),
    )
    .expect("replica");
    let control = ThreadControl {
        version: 1,
        spool,
        actor: CollaborationActor {
            principal_id: Uuid::from_u128(5084),
            agent_id: Some("original-agent".into()),
        },
        authority_digest: ContentHash::compute_typed(
            AUTHORITY_FORMAT,
            b"retained original envelope",
        ),
        authority_envelope: b"retained original envelope".to_vec(),
        client_operation_id: Uuid::from_u128(5085),
        occurred_at_ms: 1,
        control: Control::Name("agent authored".into()),
    };
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: BTreeSet::new(),
        publisher: author.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Metadata(control.encode().expect("control")),
    };
    let original = SignedOperation::sign(&operation, &author).expect("original signature");
    let id = operation.id().expect("ID");
    let statement = ThreadAuthorityAdmission {
        version: 3,
        basis: objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,
        spool,
        spool_genesis: ContentHash::compute(b"selected Spool genesis"),
        thread: operation.thread,
        subject: objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(
            id,
        ),
        actor: control.actor,
        publisher: operation.publisher,
        authority_digest: control.authority_digest,
        executor: executor.public_key().try_into().expect("key"),
        admitted_at_ms: 2000,
    };
    let receipt = SignedAuthorityAdmission::sign(&statement, &executor).expect("receipt signature");
    let batch = crate::authority_admission::batches(
        [ReceivedOperation {
            native_authority: None,
            original: original.clone(),
            authority_admission: Some(receipt.clone()),
            import_authority: None,
        }],
        1024 * 1024,
        128,
    )
    .expect("batch bounds")
    .next()
    .expect("one batch")
    .expect("structural evidence");
    let matched =
        crate::authority_admission::match_batch(&batch).expect("exact structural evidence");
    assert_eq!(matched[0].original, original);
    assert_eq!(matched[0].authority_admission.as_ref(), Some(&receipt));
    let mut duplicate = batch.clone();
    duplicate
        .authority_admissions
        .push(batch.authority_admissions[0].clone());
    assert!(crate::authority_admission::match_batch(&duplicate).is_err());
    let mut unmatched = batch.clone();
    let mut other = statement;
    other.subject =
        objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(
            ContentHash::from_bytes([87; 32]),
        );
    unmatched
        .authority_admissions
        .push(crate::authority_admission::sign(&other, &executor).expect("other signed testimony"));
    assert!(crate::authority_admission::match_batch(&unmatched).is_err());
    let local = LocalReplica::new(replica.clone(), Arc::new(repository.store().clone()));
    let generation = replica.generation().expect("generation");
    assert!(matches!(
        local.receive(matched[0].clone()).await,
        Err(native::Error::HostedTrustRequired)
    ));
    assert!(replica.operation(&id).expect("lookup").is_none());
    assert_eq!(replica.generation().expect("generation"), generation);

    // Genuine independently owned source remains available with no testimony.
    let state = objects::object::State::new_snapshot(
        objects::object::Tree::new().hash(),
        vec![genesis.base],
        objects::object::Attribution::human(objects::object::Principal::new("local owner", "")),
    );
    let source = ThreadOperation {
        body: ThreadOperationBody::Capture(
            objects::object::thread_replication::AuthoredCapture::local(
                state.encode_current_msgpack().expect("state").into(),
            ),
        ),
        ..operation
    };
    let source = SignedOperation::sign(&source, &author).expect("owned source");
    assert_eq!(
        local
            .receive(ReceivedOperation::from(source))
            .await
            .expect("local owner control"),
        Admission::Accepted
    );
}
