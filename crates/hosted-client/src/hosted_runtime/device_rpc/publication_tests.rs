//! Actual private-device publication proves first admission, replay, original
//! author rejection, and scratch cleanup with genuine standalone source packs.
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use objects::{
    object::{
        Attribution, Blob, Principal, State, Tree, TreeEntry,
        thread_replication::{AuthoredCapture, ThreadOperation, ThreadOperationBody},
    },
    store::ObjectStore,
};
use thread_api::publication::{PublicationOptions, PublicationOriginals, SourceBudget, SourcePack};

use super::*;
pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    source_peer: [u8; 32],
) {
    let base = repository.head().expect("head").expect("base");
    let replica = repository
        .create_native_thread(
            "incoming-publication",
            base,
            None,
            "private device transfer",
        )
        .expect("Thread");
    let genesis = replica.genesis().expect("genesis");
    let signer = super::checkout::signer(repository).expect("actual local signer");
    let reference = ThreadRef {
        spool: Some(SpoolRef {
            id: genesis.spool.clone(),
        }),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    let baseline = scratch_count(repository);
    for round in 0..2 {
        let blob = Blob::from(format!("private source {round}\n").into_bytes());
        repository.store().put_blob(&blob).expect("blob");
        let mut tree = Tree::new();
        tree.insert(TreeEntry::file("published.txt", blob.hash(), false).expect("path"));
        repository.store().put_tree(&tree).expect("tree");
        let state = State::new_snapshot(
            tree.hash(),
            vec![base],
            Attribution::human(Principal::new("local author", "")),
        );
        let account_signer = Ed25519Signer::from_seed(&[71; 32]).expect("original account signer");
        let original_signer = if round == 0 { &signer } else { &account_signer };
        let result = replica.prepare_capture(repository, &state).expect("references");
        let capture = if round == 0 {
            AuthoredCapture::local(result)
        } else {
            let now = chrono::Utc::now().timestamp();
            let authority = repo::device_authority::load(&repo::identity::heddle_home_dir(), now)
                .expect("independent original owner");
            let minted = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32])
                .expect("original source root");
            let key = biscuit_verifier::PublicKey::from_bytes(
                original_signer.public_key(), biscuit_auth::Algorithm::Ed25519,
            ).expect("source mint");
            let token = biscuit_verifier::parse_token(&minted.token, &[key]).expect("source Biscuit");
            let proof = repo::thread_replication::metadata::prepare_control_authority(
                &authority, &original_signer.public_key().try_into().expect("key"), &token, now,
            ).expect("sealed original source authority");
            AuthoredCapture::account(result, genesis.spool.parse().expect("Spool"),
                objects::object::CollaborationActor {
                    principal_id: uuid::Uuid::from_bytes([9; 16]), agent_id: None,
                }, proof).expect("original account capture")
        };
        let native = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: Default::default(),
            publisher: original_signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(capture),
        };
        let original = SignedOperation::sign(&native, original_signer).expect("capture");
        let refs = native
            .reference_proof(&genesis)
            .expect("reference proof")
            .into_iter()
            .collect::<Vec<_>>();
        let scratch = tempfile::tempdir().expect("sender scratch");
        let pack = SourcePack::prepare_with_references(
            repository.store(),
            &state,
            &refs,
            scratch.path(),
            SourceBudget {
                max_objects: 100_000,
                max_decoded_bytes: 256 * 1024 * 1024,
            },
        )
        .expect("genuine source pack");
        assert!(
            !replica
                .has_source_possession(state.id())
                .expect("source absent")
        );
        assert!(
            replica
                .operation(&native.id().expect("id"))
                .expect("operation absent")
                .is_none()
        );
        let originals = PublicationOriginals {
            geneses: vec![replica.genesis_record().expect("creator wrapper")],
            operations: vec![ReplicationOperations {
                operations: vec![super::thread::signed_record(&original).expect("original wire")],
                authority_admissions: vec![],
            }],
        };
        let operation = uuid::Uuid::new_v4().to_string();
        let options = || PublicationOptions {
            client_operation_id: operation.clone(),
            source: EndpointRef {
                kind: EndpointKind::Device as i32,
                public_key: source_peer.to_vec(),
            },
            sharing_policy_version: vec![],
            checkpoint: None,
        };
        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            remote
                .thread(reference.clone())
                .publish_source(&pack, &originals, options()),
        )
        .await
        .expect("publication bounded")
        .expect("private-device source publication");
        assert!(matches!(
            receipt.outcome,
            Some(publication_receipt::Outcome::Accepted(_))
        ));
        assert!(
            replica
                .has_source_possession(state.id())
                .expect("available")
        );
        assert!(matches!(
            replica
                .operation(&native.id().expect("id"))
                .expect("operation")
                .expect("accepted original")
                .1,
            objects::object::thread_replication::Admission::Accepted
        ));
        let version = replica.generation().expect("generation");
        assert_eq!(
            remote
                .thread(reference.clone())
                .publish_source(&pack, &originals, options())
                .await
                .expect("exact publication retry"),
            receipt
        );
        assert_eq!(
            replica.generation().expect("generation"),
            version,
            "receipt replay never re-admits originals or availability"
        );
        // Cryptographically valid source with the same complete pack and Thread,
        // but a different original author, cannot borrow the courier's authority.
        let foreign = Ed25519Signer::from_seed(&[118; 32]).expect("different signer");
        let denied_native = ThreadOperation {
            publisher: foreign.public_key().try_into().expect("key"),
            ..native
        };
        let denied =
            SignedOperation::sign(&denied_native, &foreign).expect("valid other signature");
        let wrong = PublicationOriginals {
            geneses: originals.geneses.clone(),
            operations: vec![ReplicationOperations {
                operations: vec![super::thread::signed_record(&denied).expect("wire")],
                authority_admissions: vec![],
            }],
        };
        let mut changed = options();
        changed.client_operation_id = uuid::Uuid::new_v4().to_string();
        assert!(
            remote
                .thread(reference.clone())
                .publish_source(&pack, &wrong, changed)
                .await
                .is_err(),
            "unproven original author must not inherit courier permission"
        );
        assert!(
            replica
                .operation(&denied_native.id().expect("id"))
                .expect("no foreign original")
                .is_none(),
            "denied original never enters causal replica"
        );
        drop(pack);
        assert_eq!(
            std::fs::read_dir(scratch.path())
                .expect("sender scratch entries")
                .count(),
            0,
            "sender removes both prepared artifacts"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while scratch_count(repository) != baseline {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("receiver staging cleanup after success and rejection");
    }
}
fn scratch_count(repository: &repo::Repository) -> usize {
    std::fs::read_dir(repository.heddle_dir())
        .expect("metadata entries")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("device-publication-")
        })
        .count()
}
