//! Genuine original/receipt signatures are structural evidence. A real
//! authenticated browser stream cannot use them as hosted witness authority.
use std::{sync::Arc, time::Duration};

use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use objects::object::{
    CollaborationActor, ContentHash,
    thread_authority_admission::ThreadAuthorityAdmission,
    thread_replication::{
        OPERATION_FORMAT, ThreadOperation, ThreadOperationBody,
        integration::{SPOOL_GENESIS_TRUST_FORMAT, TrustedHostedExecutor},
        metadata::{AUTHORITY_FORMAT, Control, Intent, ThreadControl},
    },
};
use thread_api::{Remote, credentials::Credentials, transport::IrohTransport};

use super::*;

pub(super) async fn roundtrip(
    remote: &Remote<IrohTransport<Credentials>>,
    device: &Arc<DeviceRpc>,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    browser: [u8; 32],
) {
    let genesis = replica.genesis().expect("Thread");
    let spool: uuid::Uuid = genesis.spool.parse().expect("Spool");
    let owner = Ed25519Signer::from_seed(&[71; 32]).expect("owning root");
    let executor = Ed25519Signer::from_seed(&[91; 32]).expect("structural executor signature");
    let foreign = Ed25519Signer::from_seed(&[92; 32]).expect("another account's original agent");
    let owner_genesis =
        repo::sign_spool_owner_genesis(&owner, *spool.as_bytes()).expect("owner genesis");
    repository
        .verify_and_pin_owner_genesis(
            2,
            Some(&owner_genesis.encode_to_vec()),
            &["selected".into(), "hosted-spool".into()],
        )
        .expect("selected authenticated remote owner");
    let envelope =
        b"sealed original authority checked by the enrolled host at first durable receipt".to_vec();
    let control = ThreadControl {
        version: 1,
        spool,
        actor: CollaborationActor {
            principal_id: uuid::Uuid::from_bytes([13; 16]),
            agent_id: Some("foreign-original-agent".into()),
        },
        authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
        authority_envelope: envelope,
        client_operation_id: uuid::Uuid::now_v7(),
        occurred_at_ms: 1,
        control: Control::Intent(Intent {
            outcome: "shared original agent work".into(),
            acceptance_criteria: vec![],
            origin_urls: vec![],
            principal_approved: false,
        }),
    };
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: Default::default(),
        publisher: foreign.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Metadata(control.encode().expect("control")),
    };
    let original = SignedOperation::sign(&operation, &foreign).expect("original agent signature");
    let id = operation.id().expect("ID");
    let statement = ThreadAuthorityAdmission {
        version: 3,
        basis: objects::object::original_boundary_acceptance::AdmissionBasis::OriginalAuthority,
        spool,
        spool_genesis: ContentHash::compute_typed(
            SPOOL_GENESIS_TRUST_FORMAT,
            &owner_genesis
                .genesis
                .as_ref()
                .expect("owner body")
                .encode_to_vec(),
        ),
        thread: operation.thread,
        subject: objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(
            id,
        ),
        actor: control.actor,
        publisher: operation.publisher,
        authority_digest: control.authority_digest,
        executor: executor.public_key().try_into().expect("key"),
        admitted_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    let receipt =
        thread_api::authority_admission::sign(&statement, &executor).expect("executor receipt");
    let structural = TrustedHostedExecutor {
        spool,
        spool_genesis: statement.spool_genesis,
        executor: executor.public_key().try_into().expect("key"),
    };
    thread_api::authority_admission::verify(&receipt, &original, &structural)
        .expect("genuine original and matching executor signature control");
    let generation = replica.generation().expect("generation");
    let local = thread_api::replication::native::LocalReplica::new(
        replica.clone(),
        Arc::new(objects::store::FsStore::new(repository.heddle_dir())),
    );
    use thread_api::replication::store::ReplicaStore;
    assert!(matches!(
        local
            .receive(thread_api::replication::store::ReceivedOperation {
                original: original.clone(),
                authority_admission: Some(
                    thread_api::authority_admission::decode(&receipt).expect("receipt")
                ),
                import_authority: None,
            })
            .await,
        Err(thread_api::replication::native::Error::HostedTrustRequired)
    ));
    assert_eq!(
        replica.generation().expect("no durable admission"),
        generation
    );
    let wire_original = SignedRecord {
        format: OPERATION_FORMAT.into(),
        canonical_record: original.canonical.clone(),
        signatures: vec![RecordSignature {
            public_key: operation.publisher.to_vec(),
            signature: original.signature.clone(),
        }],
    };
    drop(foreign);
    drop(executor);
    drop(owner);
    let (mut input, mut output) = remote
        .api
        .exchange::<thread_api::rpc::SyncServiceReplicateThread>(&ReplicateThreadRequest {
            body: Some(replicate_thread_request::Body::Open(ReplicationOpen {
                thread: Some(ThreadRef {
                    spool: Some(SpoolRef {
                        id: spool.to_string(),
                    }),
                    id: Some(ThreadId {
                        value: replica.thread_id().as_bytes().to_vec(),
                    }),
                }),
                facets: vec![SharedFacet::Metadata as i32],
                record_formats: vec![OPERATION_FORMAT.into()],
                session_nonce: uuid::Uuid::now_v7().as_bytes().to_vec(),
                source: Some(EndpointRef {
                    public_key: browser.to_vec(),
                    kind: EndpointKind::Device as i32,
                }),
                destination: Some(device.endpoint()),
                ..Default::default()
            })),
        })
        .await
        .expect("real authenticated browser stream");
    let ready = tokio::time::timeout(Duration::from_secs(5), output.next())
        .await
        .expect("Ready deadline")
        .expect("Ready protocol")
        .expect("Ready");
    assert!(matches!(
        ready.body,
        Some(replicate_thread_response::Body::Ready(_))
    ));
    assert!(replica.operation(&id).expect("lookup").is_none());
    input
        .send(&ReplicateThreadRequest {
            body: Some(replicate_thread_request::Body::Operations(
                ReplicationOperations {
                    import_authority: None,
                    boundary_acceptances: Vec::new(),
                    operations: vec![wire_original.clone()],
                    authority_admissions: vec![receipt.clone()],
                },
            )),
        })
        .await
        .expect("relay exact original plus independent receipt");
    let refused = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = match output.next().await {
                Err(error) => break error,
                Ok(Some(frame)) => frame,
                Ok(None) => panic!("rejected authority must terminate with a transport error"),
            };
            if let Some(replicate_thread_response::Body::Receipt(value)) = frame.body {
                assert!(
                    !value
                        .accepted_operation_ids
                        .contains(&id.as_bytes().to_vec()),
                    "structural executor signature must never admit a foreign original"
                );
            }
        }
    })
    .await
    .expect("authority rejection deadline");
    assert!(
        matches!(
            refused,
            api::v2::client::ClientError::Transport(thread_api::transport::Error::Io(ref reason))
                if reason.starts_with("stream reset by peer")
        ),
        "{refused}"
    );
    assert!(replica.operation(&id).expect("durable read").is_none());
    assert_eq!(
        replica.generation().expect("no durable admission"),
        generation
    );
    assert_eq!(repository.head().expect("checkout"), Some(genesis.base));
    drop(input);
    drop(output);
}
