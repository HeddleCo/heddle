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
        integration::SPOOL_GENESIS_TRUST_FORMAT,
        metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
    },
};
use prost::Message;
use repo::{Repository, thread_replication::ThreadReplica};
use uuid::Uuid;

use super::{native::LocalReplica, *};

#[tokio::test]
async fn original_authority_sidecars_match_exactly_and_require_fresh_witness_evidence() {
    let left_dir = tempfile::tempdir().expect("left");
    let right_dir = tempfile::tempdir().expect("right");
    let third_dir = tempfile::tempdir().expect("third");
    let missing_dir = tempfile::tempdir().expect("missing proof");
    let left_repo = Repository::init_default(left_dir.path()).expect("left repo");
    let right_repo = Repository::init_default(right_dir.path()).expect("right repo");
    let third_repo = Repository::init_default(third_dir.path()).expect("third repo");
    let missing_repo = Repository::init_default(missing_dir.path()).expect("missing repo");
    let author = Ed25519Signer::from_seed(&[81; 32]).expect("original foreign agent publisher");
    let executor = Ed25519Signer::from_seed(&[82; 32]).expect("enrolled executor");
    let owner = Ed25519Signer::from_seed(&[83; 32]).expect("Spool owner");
    let spool = Uuid::from_u128(4084);
    let genesis = ThreadGenesis {
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base: left_repo.head().expect("head").expect("base"),
        name: "shared receipt".into(),
        intent: "original authorship survives every relay".into(),
        creator: author.public_key().try_into().expect("key"),
        owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
            author.public_key().try_into().expect("key"),
        ),
        nonce: vec![85; 32],
    };
    let signed_genesis = SignedGenesis::sign(&genesis, &author).expect("genesis");
    let owner_genesis =
        repo::sign_spool_owner_genesis(&owner, *spool.as_bytes()).expect("selected owner genesis");
    let create = |repository: &Repository| {
        let replica =
            ThreadReplica::create(repository.heddle_dir(), &signed_genesis).expect("replica");
        repository
            .verify_and_pin_owner_genesis(
                2,
                Some(&owner_genesis.encode_to_vec()),
                &["selected".into(), "shared".into()],
            )
            .expect("independent owner enrollment");
        repository
            .pin_thread_hosted_executor(
                &replica,
                executor.public_key().try_into().expect("executor key"),
            )
            .expect_err("endpoint enrollment cannot substitute for fresh witness evidence");
        replica
            .set_sharing([86; 32], &BTreeSet::from([ThreadFacet::Metadata]))
            .expect("explicit local opt-in");
        replica
    };
    let left = create(&left_repo);
    let right = create(&right_repo);
    let third = create(&third_repo);
    let missing = create(&missing_repo);
    let envelope = b"only the original host held the valid first-admission credential".to_vec();
    let control = ThreadControl {
        version: 1,
        spool,
        actor: CollaborationActor {
            principal_id: Uuid::from_u128(5084),
            agent_id: Some("original-agent".into()),
        },
        authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
        authority_envelope: envelope,
        client_operation_id: Uuid::from_u128(5085),
        occurred_at_ms: 1,
        control: Control::Name("agent authored".into()),
    };
    let operation = ThreadOperation {
        version: 1,
        thread: left.thread_id(),
        parents: BTreeSet::new(),
        publisher: author.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Metadata(control.encode().expect("control")),
    };
    let original = SignedOperation::sign(&operation, &author).expect("original agent signature");
    let id = operation.id().expect("ID");
    let statement = ThreadAuthorityAdmission {
        version: 3,
            basis: heddle_object_model::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,
        spool,
        spool_genesis: ContentHash::compute_typed(
            SPOOL_GENESIS_TRUST_FORMAT,
            &owner_genesis
                .genesis
                .as_ref()
                .expect("body")
                .encode_to_vec(),
        ),
        thread: operation.thread,
        subject: objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(id),
        actor: control.actor,
        publisher: operation.publisher,
        authority_digest: control.authority_digest,
        executor: executor.public_key().try_into().expect("key"),
        admitted_at_ms: 2000,
    };
    let receipt = SignedAuthorityAdmission::sign(&statement, &executor).expect("first admission");
    let mut unknown = statement;
    unknown.subject =
        objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(
            ContentHash::from_bytes([87; 32]),
        );
    let unmatched =
        crate::authority_admission::sign(&unknown, &executor).expect("valid unmatched testimony");
    let trust = objects::object::thread_replication::integration::TrustedHostedExecutor {
        spool,
        spool_genesis: receipt.verify_signature().expect("receipt").spool_genesis,
        executor: executor.public_key().try_into().expect("key"),
    };
    receipt
        .verify(&original, &trust)
        .expect("genuine original and receipt signature control");
    assert!(matches!(
        left.receive_with_authority_admission(&original, &receipt, left_repo.store(), |_| Ok(())),
        Err(repo::thread_replication::Error::WitnessEvidenceRequired)
    ));
    drop(author);
    drop(executor);
    drop(owner);
    let session = |replica: ThreadReplica, repository: &Repository| {
        Session::new(
            LocalReplica::new(replica, Arc::new(repository.store().clone())),
            [86; 32],
            BTreeSet::from([ThreadFacet::Metadata]),
            8,
        )
        .expect("session without original credential or signing keys")
    };
    let batch = crate::contract::ReplicationOperations {
        operations: vec![crate::contract::SignedRecord {
            format: objects::object::thread_replication::OPERATION_FORMAT.into(),
            canonical_record: original.canonical.clone(),
            signatures: vec![crate::contract::RecordSignature {
                public_key: operation.publisher.to_vec(),
                signature: original.signature.clone(),
            }],
        }],
        authority_admissions: vec![
            crate::authority_admission::encode(&receipt).expect("unchanged receipt"),
        ],
        ..Default::default()
    };
    crate::authority_admission::match_batch(&batch).expect("exact sidecar matching control");
    let mut receiver = session(right.clone(), &right_repo);
    let mut duplicate = batch.clone();
    duplicate
        .authority_admissions
        .push(duplicate.authority_admissions[0].clone());
    assert!(
        receiver
            .handle(Frame::Operations(duplicate))
            .await
            .err()
            .unwrap_or_else(|| panic!("duplicate testimony rejected before storage"))
            .to_string()
            .contains("duplicate authority admission sidecar")
    );
    assert!(right.operation(&id).expect("lookup").is_none());
    let mut extra = batch.clone();
    extra.authority_admissions.push(unmatched);
    assert!(
        receiver
            .handle(Frame::Operations(extra))
            .await
            .err()
            .unwrap_or_else(|| panic!("unmatched testimony rejected before storage"))
            .to_string()
            .contains("unmatched authority admission sidecar")
    );
    assert!(right.operation(&id).expect("lookup").is_none());
    let mut no_proof = batch.clone();
    no_proof.authority_admissions.clear();
    assert!(
        session(missing.clone(), &missing_repo)
            .handle(Frame::Operations(no_proof))
            .await
            .err()
            .unwrap_or_else(|| panic!("valid signature alone cannot admit foreign account"))
            .to_string()
            .contains("independently enrolled account authority")
    );
    assert!(missing.operation(&id).expect("lookup").is_none());
    let error = receiver
        .handle(Frame::Operations(batch.clone()))
        .await
        .err()
        .expect("fresh evidence required");
    assert!(error.to_string().contains("hosted installation requires fresh root-authenticated witness and original authority evidence"), "{error}");
    assert!(right.operation(&id).expect("no mutation").is_none());
    let restarted =
        ThreadReplica::open(right_repo.heddle_dir(), operation.thread).expect("restart");
    for (replica, repository) in [(restarted, &right_repo), (third.clone(), &third_repo)] {
        let error = session(replica.clone(), repository)
            .handle(Frame::Operations(batch.clone()))
            .await
            .err()
            .expect("retained old pin cannot authorize replay");
        assert!(error.to_string().contains("hosted installation requires fresh root-authenticated witness and original authority evidence"), "{error}");
        assert!(
            replica
                .operation(&id)
                .expect("unchanged after replay")
                .is_none()
        );
    }
}
