//! Real Iroh selected source proofs include foreign integration ancestry and
//! never use target access to grant unrelated Thread ownership.
use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        Attribution, Principal, State,
        thread_replication::{
            Admission, GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    },
    store::ObjectStore,
};

use super::*;
use crate::hosted_runtime::root_mint::mint_agent_root;
pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
) {
    transfer(remote, repository, replica, endpoint_signer, owner, true).await;
}
pub(super) async fn claimed_roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
) {
    transfer(remote, repository, replica, endpoint_signer, owner, false).await;
}
async fn transfer(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
    integrated: bool,
) {
    let genesis = replica.genesis().expect("selected genesis");
    let selected = *replica
        .view()
        .expect("source view")
        .source_heads
        .first()
        .expect("selected source");
    let request = open(&genesis, selected);
    use base64::Engine as _;
    let root =
        Ed25519Signer::from_seed(&[71; 32]).expect("independently enrolled account authority");
    assert_ne!(
        root.public_key(),
        endpoint_signer.public_key(),
        "transport identity is distinct from mint authority"
    );
    let seed_credential = mint_agent_root(&[71; 32]).expect("current endpoint binding authority");
    let minted = crate::hosted_runtime::root_mint::remint_stored_root(
        &seed_credential.private_key_pem,
        &seed_credential.subject,
        None,
        Some("source-transfer-endpoint-binding-only"),
    )
    .expect("distinct authority-signed binding session");
    let binding_session = crate::hosted_runtime::root_mint::authority_session_fact(&minted.token)
        .expect("exact session identity");
    let credential = base64::engine::general_purpose::URL_SAFE
        .decode(minted.token)
        .expect("private binding bytes");
    let now = chrono::Utc::now().timestamp();
    let endpoint = EndpointRef {
        kind: EndpointKind::Device as i32,
        public_key: endpoint_signer.public_key().to_vec(),
    };
    let binding = thread_api::root_attachment::sign_binding(
        &root,
        endpoint_signer,
        RootAttachmentBinding {
            format_version: 2,
            account_id: owner.owner.as_ref().expect("account").id.clone(),
            root_public_key: root.public_key().to_vec(),
            subject_public_key: root.public_key().to_vec(),
            device: Some(endpoint),
            credential_digest: blake3::hash(&credential).as_bytes().to_vec(),
            not_before_unix_seconds: now - 1,
            expires_at_unix_seconds: now + 300,
            pairing_challenge: vec![91; 32],
        },
    )
    .expect("account and actual endpoint prove possession");
    let authority = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![],
        revoked_ids: vec![],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };

    for _ in 0..2 {
        let download = remote
            .fetch_content(request.clone(), Default::default())
            .await
            .expect("owned device exact source Fetch");
        assert!(
            download.ready().owner_genesis.is_none(),
            "device-only source does not require Weft ownership"
        );
        let scratch = tempfile::tempdir().expect("download staging");
        let staged = download
            .stage(scratch.path())
            .await
            .expect("complete source closure");
        assert_eq!(staged.state().id(), selected);
        assert_eq!(
            staged.dependency_geneses().len(),
            usize::from(integrated),
            "original integrated source Thread is portable"
        );
        let operations = staged.operations();
        assert!(!operations.is_empty());
        if integrated {
            let (position, integration) = operations
                .iter()
                .enumerate()
                .find_map(|(index, signed)| {
                    let operation = signed.verify().expect("original");
                    if operation.thread != genesis.id().expect("selected Thread") {
                        return None;
                    }
                    operation
                        .local_integration()
                        .expect("typed integration")
                        .map(|receipt| (index, receipt))
                })
                .expect("selected integration original");
            let source_position = operations
                .iter()
                .position(|signed| {
                    signed
                        .verify()
                        .expect("source original")
                        .id()
                        .expect("original ID")
                        == integration.source_operation
                })
                .expect("original integrated source proof");
            assert!(
                source_position < position,
                "source dependency must install before integration; later claim cutoff may follow both"
            );
        }
        let paths = staged.artifact_paths();
        assert!(paths.iter().all(|p| p.exists()));
        let receiver = tempfile::tempdir().expect("new receiving device repository");
        let receiving_repository = repo::Repository::init(receiver.path()).expect("receiver");
        assert_eq!(
            staged
                .install_owned_device(
                    &receiving_repository,
                    &authority,
                    thread_api::fetch::OwnedDeviceBinding {
                        attachment: &binding,
                        credential: &credential
                    },
                    &format!("spool/{}", genesis.spool),
                    now
                )
                .expect("actual independently bound device installation"),
            selected
        );
        let installed = repo::thread_replication::ThreadReplica::open(
            receiving_repository.heddle_dir(),
            genesis.id().expect("Thread"),
        )
        .expect("installed original Thread");
        assert!(
            installed
                .has_source_possession(selected)
                .expect("durable source availability")
        );
        assert_eq!(installed.genesis().expect("immutable identity"), genesis);
        if !integrated {
            assert!(
                matches!(
                    installed
                        .effective_owner()
                        .expect("explicit signed claim installed"),
                    GenesisOwner::Account(_)
                ),
                "claim must be applied after cutoff proof"
            );
            assert_eq!(
                installed.ownership_claims().expect("retained claim"),
                replica.ownership_claims().expect("source claim")
            );
        }

        assert!(
            paths.iter().all(|p| !p.exists()),
            "staging cleanup removes both artifacts"
        );
    }
    let roots = biscuit_verifier::parse_ed25519_public_keys_hex(&hex::encode(root.public_key()), 1)
        .expect("independent mint root");
    let checked = thread_api::root_attachment::verify(
        &binding,
        &credential,
        &roots,
        &owner.owner.as_ref().expect("account").id,
        binding.device.as_ref().expect("endpoint"),
        chrono::DateTime::from_timestamp(now, 0).expect("clock"),
    )
    .expect("bound credential IDs");
    assert!(
        checked
            .credential_revocation_ids()
            .contains(&binding_session),
        "binding retains exact session revocation identity"
    );
    let revoked = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![],
        revoked_ids: vec![binding_session.clone()],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };
    let scratch = tempfile::tempdir().expect("revoked staging");
    let staged = remote
        .fetch_content(request.clone(), Default::default())
        .await
        .expect("same source current courier")
        .stage(scratch.path())
        .await
        .expect("complete original source");
    let receiver = tempfile::tempdir().expect("revoked receiver");
    let receiving_repository = repo::Repository::init(receiver.path()).expect("receiver");
    let error = staged
        .install_owned_device(
            &receiving_repository,
            &revoked,
            thread_api::fetch::OwnedDeviceBinding {
                attachment: &binding,
                credential: &credential,
            },
            &format!("spool/{}", genesis.spool),
            now,
        )
        .expect_err("explicitly revoked binding cannot install source");
    assert!(
        error
            .to_string()
            .contains("endpoint binding credential is explicitly revoked"),
        "binding revocation checked before original source installation: {error}"
    );
    let stranger = Ed25519Signer::from_seed(&[113; 32]).expect("stranger key");
    let foreign = ThreadGenesis {
        owner: GenesisOwner::LocalKey(stranger.public_key().try_into().expect("key")),
        name: "unowned exact source".into(),
        creator: stranger.public_key().try_into().expect("key"),
        nonce: vec![113],
        base: selected,
        ..genesis.clone()
    };
    let foreign_replica = repo::thread_replication::ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&foreign, &stranger).expect("original genesis"),
    )
    .expect("local signed original");
    let source = repository
        .store()
        .get_state(&selected)
        .expect("source read")
        .expect("source object");
    let state = State::new_snapshot(
        source.tree,
        vec![selected],
        Attribution::human(Principal::new("foreign author", "")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: foreign.id().expect("Thread"),
        parents: BTreeSet::new(),
        publisher: foreign.creator,
        body: ThreadOperationBody::Capture(
            objects::object::thread_replication::AuthoredCapture::local(
                state.encode_current_msgpack().expect("State").into(),
            ),
        ),
    };
    assert_eq!(
        foreign_replica
            .receive(
                &SignedOperation::sign(&operation, &stranger).expect("original source"),
                repository.store(),
                |_| Ok(())
            )
            .expect("trusted local import fixture"),
        Admission::Accepted
    );
    assert!(
        remote
            .fetch_content(open(&foreign, state.id()), Default::default())
            .await
            .is_err(),
        "same Spool and existing source object do not grant unowned Thread access"
    );
}
fn open(genesis: &ThreadGenesis, state: objects::object::StateId) -> FetchOpen {
    let spool = SpoolRef {
        id: genesis.spool.clone(),
    };
    FetchOpen {
        thread: Some(ThreadRef {
            spool: Some(spool.clone()),
            id: Some(ThreadId {
                value: genesis.id().expect("id").as_bytes().to_vec(),
            }),
        }),
        revision: Some(RevisionRef {
            spool: Some(spool),
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::v1alpha1::StateId {
                    value: state.as_bytes().to_vec(),
                },
            )),
        }),
        selection: Some(TransferSelection {
            facets: vec![SharedFacet::Source as i32],
            ..Default::default()
        }),
        ..Default::default()
    }
}
