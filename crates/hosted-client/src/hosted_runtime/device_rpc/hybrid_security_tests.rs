//! Signed production device paths and complete rejection snapshots for #1963.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use api::heddle::api::{common as host, v1alpha2::*};
use crypto::{Ed25519Signer, Signer};
use objects::{
    object::{ContentHash, StateId, Tree},
    store::ObjectStore,
};
use prost::Message;
use repo::thread_replication::{
    ThreadReplica,
    hosted_trust::{self, HostedTrust, RootSelection, SystemClock},
};
use thread_api::{
    hybrid::authority::{AcceptedHistory, SelectedAuthority},
    replication::{
        native::LocalReplica,
        store::{ReceivedOperation, ReplicaStore},
    },
    transport::Authorize,
};

use super::{DeviceRpc, auth};

pub(crate) fn record<T: Message + Default>(name: &str) -> T {
    let f: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../thread-api/tests/fixtures/hybrid-alpha27.json"
    ))
    .expect("API fixture");
    let v = f["wire_vectors"]
        .get(name)
        .or_else(|| f["signed_vectors"].get(name))
        .expect("vector");
    T::decode(
        hex::decode(v["wire_hex"].as_str().expect("hex"))
            .expect("bytes")
            .as_slice(),
    )
    .expect("record")
}
pub(crate) fn sign_set(set: &mut host::SignedHostedWitnessSetV1, seed: u8) {
    let bytes =
        api::witness_trust::set_signing_bytes(set.body.as_ref().expect("body")).expect("set bytes");
    set.body_digest = api::hybrid_codec::hash(&[&bytes]);
    set.root_signature = Ed25519Signer::from_seed(&[seed; 32])
        .expect("root")
        .sign(&bytes)
        .expect("root signature");
}
pub(crate) fn fresh_set() -> host::SignedHostedWitnessSetV1 {
    let mut set: host::SignedHostedWitnessSetV1 = record("retired_set");
    let now = chrono::Utc::now().timestamp_millis();
    let body = set.body.as_mut().expect("set body");
    body.issued_at_unix_millis = now - 1000;
    body.valid_until_unix_millis = now + 240_000;
    for entry in &mut body.entries {
        if entry.state == 1 {
            entry.active_until_unix_millis = now + 300_000;
        }
    }
    let key = Ed25519Signer::from_seed(&[29; 32])
        .expect("revoked witness key")
        .public_key()
        .to_vec();
    body.entries.push(host::HostedWitnessEntryV1 {
        executor_id: api::witness_trust::witness_id(&key),
        public_key: key,
        role: 1,
        state: 3,
        purposes: vec![1, 2, 3, 4],
        active_from_unix_millis: 0,
        active_until_unix_millis: 1_300_000,
        revoked_at_unix_millis: 1_300_000,
        ..Default::default()
    });
    body.entries
        .sort_by(|a, b| a.executor_id.cmp(&b.executor_id));
    sign_set(&mut set, 7);
    set
}
pub(crate) struct Fixture {
    previous_home: Option<std::ffi::OsString>,
    pub home: tempfile::TempDir,
    _root: tempfile::TempDir,
    _source: tempfile::TempDir,
    pub repository: repo::Repository,
    pub source: repo::Repository,
    pub bundle: ImportPublicProofBundleV1,
    pub native_bundle: Option<NativePublicProofBundleV1>,
    pub root: RootSelection,
    pub original: crypto::thread_operation::SignedOperation,
    pub genesis: ThreadGenesisRecord,
    pub replica: ThreadReplica,
    pub credentials: thread_api::credentials::Credentials,
}
impl Fixture {
    pub fn new() -> Self {
        Self::build(false)
    }
    pub fn native() -> Self {
        Self::build(true)
    }
    fn build(native: bool) -> Self {
        let home = tempfile::tempdir().expect("home");
        let previous_home = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
        }
        let root_dir = tempfile::tempdir().expect("receiver");
        let source = tempfile::tempdir().expect("source");
        let repository = repo::Repository::init(root_dir.path()).expect("receiver");
        let source_repo = repo::Repository::init(source.path()).expect("source");
        let mut bundle: ImportPublicProofBundleV1 = record("complete_renewed_export");
        bundle.history_proofs = [
            "genesis_proof",
            "genesis_dev_proof",
            "publication_proof",
            "renewed_publication_proof",
        ]
        .map(record)
        .to_vec();
        bundle.witness_set = Some(fresh_set());
        let native_bundle = native.then(|| {
            let f: serde_json::Value = serde_json::from_str(include_str!(
                "../../../../thread-api/tests/fixtures/native-host-witness-v1.json"
            ))
            .expect("native vectors");
            let mut b = NativePublicProofBundleV1::decode(
                hex::decode(
                    f["wire_vectors"]["account_source"]["wire_hex"]
                        .as_str()
                        .expect("wire"),
                )
                .expect("hex")
                .as_slice(),
            )
            .expect("native bundle");
            let set = b.witness_set.as_mut().expect("native set");
            let body = set.body.as_mut().expect("body");
            let now = chrono::Utc::now().timestamp_millis();
            body.issued_at_unix_millis = now - 1000;
            body.valid_until_unix_millis = now + 240_000;
            for entry in &mut body.entries {
                entry.active_until_unix_millis = now + 300_000;
            }
            sign_set(set, 7);
            b
        });
        let history = &bundle.owner_histories[0];
        let signed_root = history.root.as_ref().expect("owner root");
        let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
            .expect("limits");
        let pinned = heddleco_capability_verifier::verify_clone_keyring(
            CloneAuthorizationKeyring {
                format_version: 1,
                spool_uuid: bundle
                    .owner_genesis
                    .as_ref()
                    .expect("genesis")
                    .genesis
                    .as_ref()
                    .expect("body")
                    .spool_uuid
                    .clone(),
                canonical_spool_path_segments: vec!["acme".into(), "imports".into()],
                pin: Some(CloneOwnerPin {
                    kind: CloneOwnerPinKind::CloneTofu as i32,
                    expected_owner_id: signed_root.root.as_ref().expect("root").owner_id.clone(),
                    first_seen_unix_seconds: 1100,
                }),
                owner_root: Some(signed_root.clone()),
                accepted_transitions: history.accepted_transitions.clone(),
                accepted_state_hash: history.state_hash.clone(),
                owner_genesis: bundle.owner_genesis.clone(),
                ownership_transfers: bundle.ownership_transfers.clone(),
                transfer_owner_histories: vec![],
            },
            chrono::Utc::now().timestamp(),
            limits,
            &[],
        )
        .expect("independent owner history");
        let owner = OwnerState {
            owner: Some(PrincipalRef {
                id: uuid::Uuid::from_bytes([0x21; 16]).to_string(),
            }),
            root: Some(signed_root.clone()),
            binding: Some(
                repo::sign_custodial_owner_binding(
                    &Ed25519Signer::from_seed(&[1; 32]).expect("owner signer"),
                    signed_root,
                    [6; 32],
                )
                .expect("account binding"),
            ),
            accepted_transitions: history.accepted_transitions.clone(),
            version: history.state_hash.clone(),
            resource_keyring: Some(pinned.wire().clone()),
            ..Default::default()
        };
        let authority = repo::device_authority::DeviceAuthority {
            owner: owner.clone(),
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        };
        repo::device_authority::publish(home.path(), &authority, chrono::Utc::now().timestamp())
            .expect("local owner enrollment");
        let spool = uuid::Uuid::from_bytes([0x23; 16]);
        repository
            .install_native_spool_id(spool)
            .expect("physical Spool identity");
        repository
            .verify_and_pin_owner_observation(
                bundle.owner_genesis.as_ref().expect("owner genesis"),
                &owner,
                spool,
                &pinned.wire().canonical_spool_path_segments,
                chrono::Utc::now().timestamp(),
            )
            .expect("independent Spool pin");
        let accepted = match &native_bundle {
            Some(b) => AcceptedHistory::from_native_spool(
                b,
                &pinned,
                chrono::Utc::now().timestamp(),
                limits,
            ),
            None => AcceptedHistory::from_selected_spool(
                &bundle,
                &pinned,
                chrono::Utc::now().timestamp(),
                limits,
            ),
        }
        .expect("accepted history");
        let root = RootSelection {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: Ed25519Signer::from_seed(&[7; 32])
                .expect("root")
                .public_key()
                .try_into()
                .expect("key"),
        };
        crate::hosted_runtime::hosted::descriptor_trust::insert_verified_pin(
            &root.authority,
            &root.root_id,
            &root.public_key,
        )
        .expect("automatic pin");
        hosted_trust::select_root(repository.heddle_dir(), &root).expect("root selection");
        hosted_trust::select_spool(
            repository.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            *accepted.genesis(),
            *accepted.initial_owner(),
        )
        .expect("Spool selection");
        let operation_record = match &native_bundle {
            Some(b) => b.authority_witnesses[0]
                .original
                .clone()
                .expect("native source"),
            None => record("converted_main"),
        };
        let original =
            thread_api::replication::decode_record(operation_record.clone()).expect("original");
        let op = original.verify().expect("native signature");
        let genesis = match &native_bundle {
            Some(b) => {
                let p = &b.genesis_witnesses[0];
                ThreadGenesisRecord {
                    genesis: p.original_genesis.clone(),
                    creator_authority: p.creator_authority_envelope.clone(),
                    native_genesis_authority: p.binding.clone(),
                    ..Default::default()
                }
            }
            None => {
                let original = bundle
                    .original_geneses
                    .iter()
                    .find(|r| {
                        objects::object::thread_replication::ThreadGenesis::decode(
                            &r.canonical_record,
                        )
                        .is_ok_and(|g| g.id().expect("ID") == op.thread)
                    })
                    .expect("original genesis")
                    .clone();
                let p = bundle
                    .genesis_witnesses
                    .iter()
                    .find(|w| w.original_genesis.as_ref() == Some(&original))
                    .expect("creator authority");
                ThreadGenesisRecord {
                    genesis: Some(original),
                    creator_authority: p.creator_authority_envelope.clone(),
                    ..Default::default()
                }
            }
        };
        let original_genesis = genesis.genesis.clone().expect("genesis");
        let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()
            .expect("seed");
        for repo in [&repository, &source_repo] {
            repo.store()
                .put_snapshot_objects_packed(Vec::new(), &Tree::new(), &seed)
                .expect("seed objects");
        }
        source_repo
            .store()
            .put_snapshot_objects_packed(
                Vec::new(),
                &Tree::new(),
                &op.source_state().expect("state").expect("capture"),
            )
            .expect("source objects");
        let trust = HostedTrust::open(repository.heddle_dir(), &root.authority, SystemClock)
            .expect("selected trust");
        if let Some(b) = &native_bundle {
            let authority = SelectedAuthority::new_native(
                accepted,
                b.clone(),
                |_: &NativePublicProofBundleV1, _: i64, _: &hosted_trust::TrustTransaction<'_>| {
                    Ok(())
                },
            );
            ThreadReplica::install_hybrid_native(
                repository.heddle_dir(),
                &trust,
                &b.encode_to_vec(),
                &[original_genesis, operation_record],
                &authority,
                source_repo.store(),
                |_| Ok(()),
            )
            .expect("witnessed native enrollment");
        } else {
            let authority = SelectedAuthority::new(
                accepted,
                bundle.clone(),
                |_: &ImportPublicProofBundleV1, _: i64, _: &hosted_trust::TrustTransaction<'_>| {
                    Ok(())
                },
            );
            ThreadReplica::install_hybrid_import(
                repository.heddle_dir(),
                &trust,
                &bundle.encode_to_vec(),
                &[original_genesis],
                &authority,
                source_repo.store(),
                |_| Ok(()),
            )
            .expect("witnessed genesis enrollment");
        }
        let replica =
            ThreadReplica::open(repository.heddle_dir(), op.thread).expect("enrolled Thread");
        let signer = Ed25519Signer::from_seed(&[1; 32]).expect("current owner signer");
        let token = crate::hosted_runtime::root_mint::mint_agent_root(&[1; 32])
            .expect("current owner capability")
            .token;
        let credentials = thread_api::credentials::Credentials::Signed {
            signer: Arc::new(signer),
            biscuit: token.into_bytes(),
            grant_envelope: vec![],
        };
        Self {
            previous_home,
            home,
            _root: root_dir,
            _source: source,
            repository,
            source: source_repo,
            bundle,
            native_bundle,
            root,
            original,
            genesis,
            replica,
            credentials,
        }
    }
    pub fn device(&self) -> DeviceRpc {
        DeviceRpc::new(self.home.path().to_path_buf(), [31; 32])
    }
    pub fn reference(&self) -> ThreadRef {
        ThreadRef {
            spool: Some(SpoolRef {
                id: uuid::Uuid::from_bytes([0x23; 16]).to_string(),
            }),
            id: Some(ThreadId {
                value: self.replica.thread_id().as_bytes().to_vec(),
            }),
        }
    }
    pub fn revision(&self) -> StateId {
        self.original
            .verify()
            .expect("original")
            .source_state()
            .expect("source")
            .expect("capture")
            .id()
    }
    pub async fn session(&self) -> auth::Session {
        let method =
            api::v2::method_descriptor("/heddle.api.v1alpha2.SyncService/Fetch").expect("Fetch");
        let body = FetchClientFrame {
            body: Some(fetch_client_frame::Body::Open(FetchOpen {
                thread: Some(self.reference()),
                ..Default::default()
            })),
        }
        .encode_to_vec();
        let context = self
            .credentials
            .context(method, &body)
            .await
            .expect("genuine signed request proof");
        let spool =
            repo::device_catalog::load(self.home.path(), uuid::Uuid::from_bytes([0x23; 16]))
                .expect("registered Spool");
        let session = auth::authorize(self.home.path(), method, &context, &body, spool)
            .expect("real authenticated Session");
        session
            .bind_thread(self.replica.thread_id())
            .expect("source authorization");
        session
    }
    pub fn local(&self) -> LocalReplica<objects::store::FsStore> {
        LocalReplica::new(
            self.replica.clone(),
            Arc::new(self.repository.store().clone()),
        )
    }
    pub fn snapshot(&self) -> Snapshot {
        snapshot(self.repository.heddle_dir())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            match &self.previous_home {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
        }
    }
}
#[derive(PartialEq)]
pub(crate) struct Snapshot {
    files: BTreeMap<PathBuf, Vec<u8>>,
    rows: BTreeMap<String, Vec<String>>,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field(
                "file_hashes",
                &self
                    .files
                    .iter()
                    .map(|(p, b)| (p, ContentHash::compute(b)))
                    .collect::<BTreeMap<_, _>>(),
            )
            .field(
                "row_hashes",
                &self
                    .rows
                    .iter()
                    .map(|(t, r)| (t, ContentHash::compute(r.join("\n").as_bytes())))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}
pub(crate) fn snapshot(directory: &Path) -> Snapshot {
    fn walk(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for e in std::fs::read_dir(path).expect("directory") {
            let e = e.expect("entry");
            if e.file_type().expect("type").is_dir() {
                walk(root, &e.path(), files)
            } else {
                files.insert(
                    e.path().strip_prefix(root).expect("relative").to_path_buf(),
                    std::fs::read(e.path()).expect("bytes"),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    for n in ["packs", "objects", "owner-authorization.bin", "spool-id"] {
        let p = directory.join(n);
        if p.is_dir() {
            walk(directory, &p, &mut files)
        } else if p.exists() {
            files.insert(n.into(), std::fs::read(p).expect("bytes"));
        }
    }
    let conn = rusqlite::Connection::open(directory.join(repo::local_metadata::DATABASE_NAME))
        .expect("SQL");
    let mut rows = BTreeMap::new();
    for table in [
        "threads",
        "operations",
        "hosted_import_admissions",
        "hosted_import_proofs",
        "hosted_import_job_keys",
        "hosted_import_slots",
        "hosted_witness_trust",
        "thread_source_availability",
        "operation_receipts",
    ] {
        let mut query = conn
            .prepare(&format!("SELECT * FROM {table}"))
            .expect("table");
        let cols = query.column_count();
        let mut values = query
            .query_map([], |r| {
                Ok((0..cols)
                    .map(|i| format!("{:?}", r.get_ref(i)))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("values");
        values.sort();
        rows.insert(table.into(), values);
    }
    Snapshot { files, rows }
}
#[tokio::test]
async fn security_f3_signed_device_backend_checks_current_authority_without_reentry() {
    let _guard = crate::test_process_env::exclusive().await;
    let f = Fixture::new();
    let session = Arc::new(f.session().await);
    session.check_current(f.home.path()).expect("preflight");
    let backend = f
        .device()
        .hosted_backend(f.local(), session, f.bundle.clone())
        .expect("real backend");
    assert_eq!(
        backend
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(f.bundle.clone()))
            })
            .await
            .expect("fully signed real device HYBRID transfer"),
        objects::object::thread_replication::Admission::Accepted
    );
    let (exported, status) = backend
        .operation(f.original.verify().expect("original").id().expect("ID"))
        .await
        .expect("fresh export")
        .expect("retained original");
    assert_eq!(
        status,
        objects::object::thread_replication::Admission::Accepted
    );
    assert_eq!(exported.original, f.original);
}
#[tokio::test]
async fn security_f3_final_device_revocation_and_expiry_restore_everything() {
    let _guard = crate::test_process_env::exclusive().await;
    for expiry in [false, true] {
        let mut f = Fixture::new();
        let expires = chrono::Utc::now() + chrono::Duration::seconds(3);
        if expiry {
            let token = crate::hosted_runtime::root_mint::mint_independent_root(
                crate::hosted_runtime::root_mint::IndependentRootMint {
                    seed: &[1; 32],
                    subject: "security-control",
                    ttl: chrono::Duration::seconds(3),
                    credential_id: None,
                    session_id: None,
                    expires_at: Some(expires),
                },
            )
            .expect("short signed capability")
            .token;
            f.credentials = thread_api::credentials::Credentials::Signed {
                signer: Arc::new(Ed25519Signer::from_seed(&[1; 32]).expect("owner")),
                biscuit: token.into_bytes(),
                grant_envelope: vec![],
            };
        }
        let mut session = f.session().await;
        let home = f.home.path().to_owned();
        let directory = f.repository.heddle_dir().to_owned();
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let checked = reached.clone();
        session.before_current_check = Some(Arc::new(move |_| {
            if directory.join("hosted-install.intent").exists() {
                assert!(
                    directory
                        .join("packs")
                        .read_dir()
                        .expect("installed packs")
                        .count()
                        > 0,
                    "final callback follows actual artifact publication"
                );
                checked.store(true, std::sync::atomic::Ordering::SeqCst);
                if expiry {
                    while chrono::Utc::now() < expires {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                } else {
                    let mut a =
                        repo::device_authority::load(&home, chrono::Utc::now().timestamp())?;
                    a.revoked_publishers.push(
                        Ed25519Signer::from_seed(&[1; 32])?
                            .public_key()
                            .try_into()
                            .expect("key"),
                    );
                    repo::device_authority::publish(&home, &a, chrono::Utc::now().timestamp())?;
                }
            }
            Ok(())
        }));
        let before = f.snapshot();
        let backend = f
            .device()
            .hosted_backend(f.local(), Arc::new(session), f.bundle.clone())
            .expect("real backend");
        let error = backend
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(f.bundle.clone())),
            })
            .await
            .expect_err("late disclosure rejection");
        assert!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            "must reach final authority hook"
        );
        assert!(
            error
                .to_string()
                .contains(if expiry { "expired" } else { "revoked" }),
            "{error}"
        );
        assert_eq!(
            f.snapshot(),
            before,
            "late real Session rejection restores artifacts, originals, possession, receipt and trust"
        );
    }
}
impl Fixture {
    pub fn pack(&self) -> (tempfile::TempDir, thread_api::publication::SourcePack) {
        let scratch = tempfile::tempdir().expect("sender staging");
        let op = self.original.verify().expect("original");
        let genesis = objects::object::thread_replication::ThreadGenesis::decode(
            &self
                .genesis
                .genesis
                .as_ref()
                .expect("genesis")
                .canonical_record,
        )
        .expect("genesis");
        let refs = op
            .reference_proof(&genesis)
            .expect("references")
            .into_iter()
            .collect::<Vec<_>>();
        let pack = thread_api::publication::SourcePack::prepare_with_references(
            self.source.store(),
            &op.source_state().expect("state").expect("capture"),
            &refs,
            scratch.path(),
            thread_api::publication::SourceBudget {
                max_objects: 100_000,
                max_decoded_bytes: 256 * 1024 * 1024,
            },
        )
        .expect("original source pack");
        (scratch, pack)
    }
    pub fn originals(&self) -> thread_api::publication::PublicationOriginals {
        let op = self.original.verify().expect("original");
        thread_api::publication::PublicationOriginals {
            geneses: vec![self.genesis.clone()],
            operations: vec![ReplicationOperations {
                native_authority: None,
                operations: vec![SignedRecord {
                    format: objects::object::thread_replication::OPERATION_FORMAT.into(),
                    canonical_record: self.original.canonical.clone(),
                    signatures: vec![RecordSignature {
                        public_key: op.publisher.to_vec(),
                        signature: self.original.signature.clone(),
                    }],
                }],
                authority_admissions: vec![],
                boundary_acceptances: vec![],
                import_authority: Some(self.bundle.clone()),
            }],
        }
    }
}
#[tokio::test]
async fn security_f2_complete_device_publication_commits_and_replays_receipt() {
    let _guard = crate::test_process_env::exclusive().await;
    let f = Fixture::new();
    use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("address")
        .bind()
        .await
        .expect("device endpoint");
    let address = endpoint.addr();
    let key = *endpoint.id().as_bytes();
    let device = Arc::new(DeviceRpc::new(f.home.path().to_owned(), key));
    let (authorization, _, _) =
        crate::hosted_runtime::claim_authorization::StoredClaimAuthorization::new();
    let authorization = Arc::new(authorization);
    let protocol = crate::hosted_runtime::hosted::claim_protocol::ClaimProtocol::new(
        authorization.clone(),
        authorization,
        key,
    )
    .with_device(device);
    let alpn = crate::hosted_runtime::hosted::claim_protocol::NATIVE_ALPN;
    let router = Router::builder(endpoint).accept(alpn, protocol).spawn();
    let browser = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("address")
        .bind()
        .await
        .expect("courier");
    let source_peer = *browser.id().as_bytes();
    let connection = browser
        .connect(address, alpn)
        .await
        .expect("real Iroh connection");
    let transport = thread_api::transport::IrohTransport::new(
        connection,
        f.credentials.clone(),
        256 * 1024,
        std::time::Duration::from_secs(30),
    )
    .expect("transport");
    let remote = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("device discovery");
    let (_scratch, pack) = f.pack();
    let originals = f.originals();
    let id = uuid::Uuid::new_v4().to_string();
    let options = || thread_api::publication::PublicationOptions {
        client_operation_id: id.clone(),
        source: EndpointRef {
            kind: EndpointKind::Device as i32,
            public_key: source_peer.to_vec(),
        },
        sharing_policy_version: vec![],
        checkpoint: None,
    };
    let receipt = remote
        .thread(f.reference())
        .publish_source(&pack, &originals, options())
        .await
        .expect("complete witnessed DeviceRpc publication");
    assert_eq!(receipt.client_operation_id, id);
    assert!(
        f.replica
            .has_source_possession(f.revision())
            .expect("possession")
    );
    assert_eq!(
        f.replica
            .operation(&f.original.verify().expect("original").id().expect("ID"))
            .expect("stored")
            .expect("accepted original")
            .0,
        f.original
    );
    assert!(
        f.replica
            .hosted_admission(f.original.verify().expect("original").id().expect("ID"))
            .expect("witnessed admission")
            .is_some()
    );
    let mut before = f.snapshot();
    let replay = remote
        .thread(f.reference())
        .publish_source(&pack, &originals, options())
        .await
        .expect("receipt replay");
    assert_eq!(receipt, replay);
    let mut after = f.snapshot();
    before.rows.remove("hosted_witness_trust");
    after.rows.remove("hosted_witness_trust");
    assert_eq!(
        before, after,
        "exact replay changes no publication state; trust only advances its clock floor"
    );
    drop(remote);
    browser.close().await;
    router.shutdown().await.expect("shutdown");
}
fn lookup(
    device: &DeviceRpc,
    set: Option<host::SignedHostedWitnessSetV1>,
    bundle: &ImportPublicProofBundleV1,
    available_proofs: bool,
) {
    let proofs = if available_proofs {
        bundle
            .statements
            .iter()
            .filter_map(|s| {
                let request = thread_api::hybrid::history::request(s).expect("lookup selector");
                let body = s.body.as_ref().expect("statement");
                bundle
                    .history_proofs
                    .iter()
                    .filter(|p| p.executor_id == body.executor_id && p.purpose == body.purpose)
                    .find(|p| {
                        let set = api::witness_trust::verify_set(
                            bundle.witness_set.as_ref().expect("fixture set"),
                            &api::witness_trust::SetExpectation {
                                authority: "https://weft.example.test",
                                root_id: "descriptor-root-1",
                                root_public_key: Ed25519Signer::from_seed(&[7; 32])
                                    .expect("root")
                                    .public_key(),
                                root_epoch: 1,
                                now_unix_millis: chrono::Utc::now().timestamp_millis(),
                                clock_floor_unix_millis: 0,
                                known_job_keys: &[],
                            },
                            None,
                        )
                        .expect("fixture trust");
                        api::witness_trust::resolve_statement(
                            &set,
                            s,
                            Some(p),
                            false,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .is_ok()
                    })
                    .map(|p| {
                        (
                            request,
                            GetHostedWitnessHistoryProofResponse {
                                proof: Some(p.clone()),
                            },
                        )
                    })
            })
            .collect()
    } else {
        vec![]
    };
    *device.test_witness_responses.lock().expect("lookup") =
        Some(crate::hosted_runtime::hosted::descriptor_trust::TestWitnessResponses { set, proofs });
}

#[tokio::test]
async fn native_f4_device_relay_refreshes_expired_metadata_without_replacing_originals() {
    let _guard = crate::test_process_env::exclusive().await;
    let f = Fixture::native();
    let device = f.device();
    let session = Arc::new(f.session().await);
    let mut expired = f.native_bundle.clone().expect("native control");
    let set = expired.witness_set.as_mut().expect("set");
    set.body.as_mut().expect("body").generation += 1;
    set.body.as_mut().expect("body").valid_until_unix_millis =
        chrono::Utc::now().timestamp_millis() + 1800;
    sign_set(set, 7);
    let backend = device
        .hosted_backend(f.local(), session.clone(), expired.clone())
        .expect("native backend");
    backend
        .recheck_native_selected(
            &expired,
            &[f.genesis.genesis.clone().expect("original genesis")],
        )
        .expect("fresh native metadata");
    let expiry = expired
        .witness_set
        .as_ref()
        .expect("set")
        .body
        .as_ref()
        .expect("body")
        .valid_until_unix_millis;
    while chrono::Utc::now().timestamp_millis() < expiry {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let id = f.original.verify().expect("original").id().expect("ID");
    assert!(
        backend.operation(id).await.is_err(),
        "expired control fails closed"
    );
    let mut successor = expired.witness_set.clone().expect("set");
    let body = successor.body.as_mut().expect("body");
    body.generation += 1;
    body.issued_at_unix_millis = chrono::Utc::now().timestamp_millis() - 1;
    body.valid_until_unix_millis = body.issued_at_unix_millis + 240_000;
    let mut leaves = expired
        .statements
        .iter()
        .map(|s| {
            let request = thread_api::hybrid::history::request(s).expect("exact leaf selector");
            (request.statement_leaf_digest.clone(), s)
        })
        .collect::<Vec<_>>();
    leaves.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(leaves.len(), 2, "genesis and account source witnesses");
    let retired = &mut body.entries[0];
    retired.state = 2;
    retired.active_until_unix_millis = expiry;
    retired.archive_leaf_count = 2;
    retired.archive_root = api::witness_trust::merkle_root(
        &leaves
            .iter()
            .map(|(leaf, _)| leaf.clone())
            .collect::<Vec<_>>(),
    )
    .expect("sealed exact history");
    let next_key = Ed25519Signer::from_seed(&[6; 32])
        .expect("next witness")
        .public_key()
        .to_vec();
    body.current_executor_id = api::witness_trust::witness_id(&next_key);
    body.entries.push(host::HostedWitnessEntryV1 {
        executor_id: body.current_executor_id.clone(),
        public_key: next_key,
        role: 1,
        state: 1,
        purposes: vec![1, 2, 3, 4],
        active_from_unix_millis: expiry,
        active_until_unix_millis: body.valid_until_unix_millis + 60_000,
        ..Default::default()
    });
    body.entries
        .sort_by(|a, b| a.executor_id.cmp(&b.executor_id));
    let proofs = leaves
        .iter()
        .enumerate()
        .map(|(index, (_, statement))| {
            let request = thread_api::hybrid::history::request(statement).expect("request");
            (
                request.clone(),
                GetHostedWitnessHistoryProofResponse {
                    proof: Some(host::HostedWitnessHistoryProofV1 {
                        executor_id: request.executor_id,
                        purpose: statement.body.as_ref().expect("statement").purpose,
                        leaf_index: index as u64,
                        leaf_count: 2,
                        siblings: vec![leaves[1 - index].0.clone()],
                    }),
                },
            )
        })
        .collect::<Vec<_>>();
    sign_set(&mut successor, 7);
    *device.test_witness_responses.lock().expect("lookup") = Some(
        crate::hosted_runtime::hosted::descriptor_trust::TestWitnessResponses {
            set: Some(successor.clone()),
            proofs: proofs.clone(),
        },
    );
    let mut refreshed = expired.clone();
    device
        .refresh_native_export_bundle(&session, &mut refreshed)
        .await
        .expect("proof-only native refresh");
    assert_eq!(
        refreshed.history_proofs.len(),
        2,
        "both exact retirement paths were recovered"
    );
    for negative in ["missing", "neighboring"] {
        let mut candidate = expired.clone();
        let mut incorrect = proofs.clone();
        if negative == "missing" {
            incorrect.clear();
        } else {
            incorrect[0].1 = incorrect[1].1.clone();
        }
        *device.test_witness_responses.lock().expect("lookup") = Some(
            crate::hosted_runtime::hosted::descriptor_trust::TestWitnessResponses {
                set: Some(successor.clone()),
                proofs: incorrect,
            },
        );
        let before = f.snapshot();
        assert!(
            device
                .refresh_native_export_bundle(&session, &mut candidate)
                .await
                .is_err(),
            "{negative} retirement proof rejects"
        );
        assert_eq!(candidate, expired, "failed preparation preserves originals");
        assert_eq!(
            before,
            f.snapshot(),
            "failed preparation preserves durable trust"
        );
        *device.test_witness_responses.lock().expect("lookup") = Some(
            crate::hosted_runtime::hosted::descriptor_trust::TestWitnessResponses {
                set: Some(successor.clone()),
                proofs: proofs.clone(),
            },
        );
        device
            .refresh_native_export_bundle(&session, &mut candidate)
            .await
            .expect("nearby exact retirement proof control");
    }
    let mut comparison = expired;
    thread_api::hybrid::history::replace_native_receiver_metadata(
        &mut comparison,
        refreshed.clone(),
    )
    .expect("every original byte unchanged");
    let files = f.snapshot().files;
    let (exported, _) = device
        .relay(f.replica.clone(), f.local(), session.clone())
        .operation(id)
        .await
        .expect("refreshed device export")
        .expect("original");
    assert_eq!(exported.original, f.original);
    assert_eq!(exported.native_authority.as_deref(), Some(&refreshed));
    assert!(exported.import_authority.is_none());
    assert_eq!(
        files,
        f.snapshot().files,
        "refresh needs no replacement content"
    );
    let mut revoked = refreshed.clone();
    let set = revoked.witness_set.as_mut().expect("set");
    let body = set.body.as_mut().expect("body");
    body.generation += 1;
    let issuer = body
        .entries
        .iter_mut()
        .find(|e| e.state == 2)
        .expect("original witness");
    issuer.state = 3;
    issuer.revoked_at_unix_millis = expiry;
    sign_set(set, 7);
    let before = f.snapshot();
    *device.test_witness_responses.lock().expect("lookup") = Some(
        crate::hosted_runtime::hosted::descriptor_trust::TestWitnessResponses {
            set: revoked.witness_set.clone(),
            proofs: vec![],
        },
    );
    assert!(
        device
            .refresh_native_export_bundle(&session, &mut revoked)
            .await
            .is_err(),
        "revoked original issuer cannot be refreshed"
    );
    assert_eq!(before, f.snapshot(), "failed refresh has no durable effect");
    *device.test_witness_responses.lock().expect("lookup") = Some(
        crate::hosted_runtime::hosted::descriptor_trust::TestWitnessResponses {
            set: Some(successor),
            proofs,
        },
    );
    device
        .relay(f.replica.clone(), f.local(), session)
        .operation(id)
        .await
        .expect("nearby unrevoked refreshed control");
}
#[tokio::test]
async fn security_f4_device_export_refreshes_expired_metadata_and_completes_retirement_proofs() {
    let _guard = crate::test_process_env::exclusive().await;
    let mut f = Fixture::new();
    let device = f.device();
    let session = Arc::new(f.session().await);
    let backend = device
        .hosted_backend(f.local(), session.clone(), f.bundle.clone())
        .expect("initial backend");
    backend
        .receive(ReceivedOperation {
            native_authority: None,
            original: f.original.clone(),
            authority_admission: None,
            import_authority: Some(Arc::new(f.bundle.clone())),
        })
        .await
        .expect("retained content");
    let mut expiring = f.bundle.clone();
    let set = expiring.witness_set.as_mut().expect("set");
    let body = set.body.as_mut().expect("body");
    body.generation += 1;
    body.valid_until_unix_millis = chrono::Utc::now().timestamp_millis() + 1800;
    sign_set(set, 7);
    let backend = device
        .hosted_backend(f.local(), session.clone(), expiring.clone())
        .expect("short-lived backend");
    backend
        .recheck_selected(&expiring, &[record("converted_main")])
        .expect("fresh shortened metadata");
    let expiry = expiring
        .witness_set
        .as_ref()
        .expect("set")
        .body
        .as_ref()
        .expect("body")
        .valid_until_unix_millis;
    while chrono::Utc::now().timestamp_millis() < expiry {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        backend
            .operation(f.original.verify().expect("original").id().expect("ID"))
            .await
            .is_err(),
        "control: unchanged stored set actually expired"
    );
    let files = f.snapshot().files;
    let mut successor = expiring.witness_set.clone().expect("set");
    let body = successor.body.as_mut().expect("body");
    body.generation += 1;
    body.issued_at_unix_millis = chrono::Utc::now().timestamp_millis() - 1;
    body.valid_until_unix_millis = body.issued_at_unix_millis + 240_000;
    sign_set(&mut successor, 7);
    lookup(&device, Some(successor.clone()), &f.bundle, true);
    let mut refreshed = expiring.clone();
    refreshed.history_proofs.clear();
    device
        .refresh_export_bundle(&session, &mut refreshed)
        .await
        .expect("device freshness-only refresh and public retirement paths");
    let mut comparison = expiring.clone();
    thread_api::hybrid::history::replace_receiver_metadata(&mut comparison, refreshed.clone())
        .expect("all original bytes preserved");
    let relay = device.relay(f.replica.clone(), f.local(), session.clone());
    let (exported, _) = relay
        .operation(f.original.verify().expect("original").id().expect("ID"))
        .await
        .expect("device export after expiry")
        .expect("original");
    assert_eq!(exported.original, f.original);
    assert_eq!(refreshed.witness_set, Some(successor));
    assert!(
        !refreshed.history_proofs.is_empty(),
        "lookup completed retired witness proofs"
    );
    assert_eq!(exported.import_authority.as_deref(), Some(&refreshed));
    assert_eq!(
        f.snapshot().files,
        files,
        "no content download or replacement required"
    );
    f.bundle = refreshed;
}
#[tokio::test]
async fn security_f4_export_refresh_negatives_have_valid_controls_and_do_not_mutate() {
    let _guard = crate::test_process_env::exclusive().await;
    for negative in [
        "retirement",
        "missing_retirement_proof",
        "neighboring_retirement_proof",
        "invalid_signature",
        "durable_n_plus_one",
        "revoked_issuer",
        "unavailable_lookup",
    ] {
        let f = Fixture::new();
        let device = f.device();
        let session = Arc::new(f.session().await);
        let mut valid = f.bundle.witness_set.clone().expect("set");
        valid.body.as_mut().expect("body").generation += 2;
        sign_set(&mut valid, 7);
        let mut candidate = f.bundle.clone();
        candidate.history_proofs.clear();
        let mut bad = valid.clone();
        match negative {
            "retirement" => {
                bad.body
                    .as_mut()
                    .expect("body")
                    .entries
                    .iter_mut()
                    .find(|e| e.state == 2)
                    .expect("seal")
                    .archive_root[0] ^= 1;
                sign_set(&mut bad, 7);
                lookup(&device, Some(bad), &f.bundle, true);
            }
            "missing_retirement_proof" => lookup(&device, Some(bad), &f.bundle, false),
            "neighboring_retirement_proof" => {
                lookup(&device, Some(bad), &f.bundle, true);
                let mut responses = device.test_witness_responses.lock().expect("lookup");
                let responses = responses.as_mut().expect("public service");
                let neighbor = responses.proofs[1].1.clone();
                responses.proofs[0].1 = neighbor;
            }
            "invalid_signature" => {
                bad.root_signature[0] ^= 1;
                lookup(&device, Some(bad), &f.bundle, true);
            }
            "durable_n_plus_one" => {
                let trust =
                    HostedTrust::open(f.repository.heddle_dir(), &f.root.authority, SystemClock)
                        .expect("trust");
                trust
                    .mutate(&valid, |_| Ok(()))
                    .expect("independent N+1 admission");
                lookup(&device, f.bundle.witness_set.clone(), &f.bundle, true);
            }
            "revoked_issuer" => {
                let e = bad
                    .body
                    .as_mut()
                    .expect("body")
                    .entries
                    .iter_mut()
                    .find(|e| e.state == 2)
                    .expect("issuer");
                e.state = 3;
                e.revoked_at_unix_millis = 1_300_000;
                sign_set(&mut bad, 7);
                lookup(&device, Some(bad), &f.bundle, true);
            }
            "unavailable_lookup" => lookup(&device, None, &f.bundle, true),
            _ => unreachable!(),
        }
        let before = f.snapshot();
        let originals = candidate.clone();
        assert!(
            device
                .refresh_export_bundle(&session, &mut candidate)
                .await
                .is_err(),
            "{negative} must reject"
        );
        assert_eq!(
            candidate, originals,
            "{negative}: preparation preserves original bytes on failure"
        );
        assert_eq!(
            f.snapshot(),
            before,
            "{negative}: failed export changes no durable state"
        );
        lookup(&device, Some(valid), &f.bundle, true);
        device
            .refresh_export_bundle(&session, &mut candidate)
            .await
            .expect("corresponding valid refreshed control");
        let backend = device
            .hosted_backend(f.local(), session, candidate.clone())
            .expect("valid selected authority");
        backend
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(candidate)),
            })
            .await
            .expect("valid refreshed transfer");
    }
}
impl Fixture {
    async fn staged(&self, endpoint: EndpointRef) -> thread_api::fetch::StagedSource {
        let (_scratch, pack) = self.pack();
        let directory = tempfile::tempdir().expect("received pack");
        for (mut file, name) in pack
            .open_artifacts()
            .await
            .expect("source artifacts")
            .into_iter()
            .zip(["source.pack", "source.idx"])
        {
            let mut output = tokio::fs::File::create(directory.path().join(name))
                .await
                .expect("received file");
            tokio::io::copy(&mut file, &mut output)
                .await
                .expect("received exact bytes");
        }
        let revision = RevisionRef {
            spool: self.reference().spool,
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::common::StateId {
                    value: self.revision().as_bytes().to_vec(),
                },
            )),
        };
        let opening = PublishContentOpen {
            thread: Some(self.reference()),
            revision: Some(revision.clone()),
            packs: pack.artifacts().to_vec(),
            destination: Some(endpoint),
            protocol: Some(thread_api::hybrid::protocol()),
            import_authority: Some(self.bundle.clone()),
            ..Default::default()
        };
        thread_api::publication::validate_source_artifacts(directory, &opening, self.originals())
            .expect("valid actual artifacts")
            .into_hosted_source(TransferReady {
                thread: Some(self.reference()),
                current: Some(revision),
                owner_genesis: self.bundle.owner_genesis.clone(),
                ownership: Some(
                    self.repository
                        .pinned_owner_observation(chrono::Utc::now().timestamp())
                        .expect("independent pin")
                        .0,
                ),
                protocol: opening.protocol,
                import_authority: Some(self.bundle.clone()),
                full_closure_available: true,
                ..Default::default()
            })
            .expect("actual hosted staging")
    }
    fn change_control(&self, sharing: bool) {
        use objects::object::thread_replication::{
            ThreadOperation, ThreadOperationBody,
            metadata::{AUTHORITY_FORMAT, Control, SharingPolicy, ThreadControl},
        };
        let now = chrono::Utc::now().timestamp();
        let authority = repo::device_authority::load(self.home.path(), now).expect("current owner");
        let signer = Ed25519Signer::from_seed(&[1; 32]).expect("original author");
        let token =
            crate::hosted_runtime::root_mint::mint_agent_root(&[1; 32]).expect("signed capability");
        let key = biscuit_verifier::PublicKey::from_bytes(
            signer.public_key(),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("mint selector");
        let token = biscuit_verifier::parse_token(&token.token, &[key]).expect("Biscuit");
        let envelope = repo::thread_replication::metadata::prepare_control_authority(
            &authority,
            &signer.public_key().try_into().expect("key"),
            &token,
            now,
        )
        .expect("current native author");
        let control = ThreadControl {
            version: 1,
            spool: uuid::Uuid::from_bytes([0x23; 16]),
            actor: objects::object::CollaborationActor {
                principal_id: uuid::Uuid::from_bytes([0x21; 16]),
                agent_id: None,
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
            authority_envelope: envelope,
            client_operation_id: uuid::Uuid::new_v4(),
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            control: if sharing {
                Control::Sharing(SharingPolicy {
                    ongoing: true,
                    destinations: vec![],
                })
            } else {
                Control::Name("concurrent-frontier".into())
            },
        };
        let op = ThreadOperation {
            version: 1,
            thread: self.replica.thread_id(),
            parents: Default::default(),
            publisher: signer.public_key().try_into().expect("publisher"),
            body: ThreadOperationBody::Metadata(control.encode().expect("control")),
        };
        let signed = crypto::thread_operation::SignedOperation::sign(&op, &signer)
            .expect("original signature");
        assert_eq!(
            self.replica
                .receive_control_cas(&signed, self.repository.store(), |op| {
                    repo::thread_replication::metadata::verify_control_authority(
                        op,
                        &authority,
                        &uuid::Uuid::from_bytes([0x23; 16]).to_string(),
                        now,
                    )
                })
                .expect("genuine concurrent control"),
            objects::object::thread_replication::Admission::Accepted
        );
    }
}
#[tokio::test]
async fn security_f2_late_publication_rejections_leave_all_authoritative_state_unchanged() {
    let _guard = crate::test_process_env::exclusive().await;
    for negative in [
        "access_revoked",
        "policy_changed",
        "frontier_changed",
        "receipt_conflict",
    ] {
        let f = Fixture::new();
        let device = f.device();
        let mut session = f.session().await;
        let guards = vec![(
            f.replica.clone(),
            f.replica.generation().expect("observed generation"),
        )];
        let policy = objects::object::thread_replication::metadata::property_version(
            f.replica.thread_id(),
            &objects::object::thread_replication::metadata::Property::Sharing,
            &Default::default(),
        )
        .expect("expected policy");
        let id = uuid::Uuid::new_v4()
            .to_string()
            .parse::<objects::object::OperationId>()
            .expect("command");
        let namespace = session
            .command_namespace()
            .expect("authenticated namespace");
        let staged = f.staged(device.endpoint()).await;
        let records = vec![f.original.clone()];
        let admissions = BTreeMap::new();
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if negative == "access_revoked" {
            let home = f.home.path().to_owned();
            let directory = f.repository.heddle_dir().to_owned();
            let reached = reached.clone();
            session.before_current_check = Some(Arc::new(move |_| {
                if directory.join("hosted-install.intent").exists() {
                    reached.store(true, std::sync::atomic::Ordering::SeqCst);
                    let mut authority =
                        repo::device_authority::load(&home, chrono::Utc::now().timestamp())?;
                    authority.revoked_publishers.push(
                        Ed25519Signer::from_seed(&[1; 32])?
                            .public_key()
                            .try_into()
                            .expect("publisher"),
                    );
                    repo::device_authority::publish(
                        &home,
                        &authority,
                        chrono::Utc::now().timestamp(),
                    )?;
                }
                Ok(())
            }));
        }
        let session = Arc::new(session);
        let backend = device
            .hosted_backend(f.local(), session.clone(), f.bundle.clone())
            .expect("production device authority");
        if negative == "policy_changed" {
            f.change_control(true)
        } else if negative == "frontier_changed" {
            f.change_control(false)
        } else if negative == "receipt_conflict" {
            let conn = rusqlite::Connection::open(
                f.repository
                    .heddle_dir()
                    .join(repo::local_metadata::DATABASE_NAME),
            )
            .expect("racing receipt");
            conn.execute("INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",rusqlite::params![namespace,id.to_string(),repo::operation_dedup::receipt_record_key(&namespace,id).as_bytes(),"/heddle.api.v1alpha2.SyncService/PublishContent",[99u8;32].as_slice(),vec![42u8],chrono::Utc::now().timestamp()]).expect("other already committed command body");
        }
        let before = f.snapshot();
        let result = backend.publish_source(
            staged,
            &f.repository,
            chrono::Utc::now().timestamp(),
            thread_api::fetch::hosted::HostedPublication {
                replica: &f.replica,
                prepared: repo::thread_replication::source_publication::PreparedPublication {
                    operations: &records,
                    authority_admissions: &admissions,
                    revision: f.revision(),
                    guards: &guards,
                },
                command: repo::thread_replication::source_publication::Command {
                    namespace: &namespace,
                    id,
                    method: "/heddle.api.v1alpha2.SyncService/PublishContent",
                    request_hash: [8; 32],
                },
            },
            |context| {
                session
                    .check_current_in(f.home.path(), context)
                    .map_err(|e| repo::thread_replication::Error::Invalid(e.to_string()))?;
                if context.property_version(
                    f.replica.thread_id(),
                    &objects::object::thread_replication::metadata::Property::Sharing,
                )? != policy
                {
                    return Err(repo::thread_replication::Error::Invalid(
                        "publication sharing policy changed".into(),
                    ));
                }
                Ok(vec![41])
            },
        );
        let error = result.expect_err("late publication rejection");
        assert!(
            error.to_string().contains(match negative {
                "access_revoked" => "revoked",
                "policy_changed" | "frontier_changed" => "frontier changed",
                "receipt_conflict" => "command ID reused",
                _ => unreachable!(),
            }),
            "{negative}: {error}"
        );
        if negative == "access_revoked" {
            assert!(
                reached.load(std::sync::atomic::Ordering::SeqCst),
                "access revocation occurs after journaled artifact publication"
            );
        }
        assert_eq!(
            f.snapshot(),
            before,
            "{negative}: artifacts, admissions, possession, receipt and trust remain unchanged"
        );
    }
}
#[tokio::test]
async fn security_f4_refresh_preparation_cannot_override_concurrent_durable_trust() {
    let _guard = crate::test_process_env::exclusive().await;
    let f = Fixture::new();
    let device = f.device();
    let session = Arc::new(f.session().await);
    let mut successor = f.bundle.witness_set.clone().expect("set");
    successor.body.as_mut().expect("body").generation += 1;
    sign_set(&mut successor, 7);
    lookup(&device, Some(successor), &f.bundle, true);
    let mut prepared = f.bundle.clone();
    device
        .refresh_export_bundle(&session, &mut prepared)
        .await
        .expect("valid asynchronous refresh");
    let backend = device
        .hosted_backend(f.local(), session.clone(), prepared.clone())
        .expect("prepared authority");
    let mut concurrent = prepared.witness_set.clone().expect("set");
    concurrent.body.as_mut().expect("body").generation += 1;
    sign_set(&mut concurrent, 7);
    let trust = HostedTrust::open(f.repository.heddle_dir(), &f.root.authority, SystemClock)
        .expect("trust");
    trust
        .mutate(&concurrent, |_| Ok(()))
        .expect("independent durable N+1");
    let before = f.snapshot();
    let error = backend
        .receive(ReceivedOperation {
            native_authority: None,
            original: f.original.clone(),
            authority_admission: None,
            import_authority: Some(Arc::new(prepared.clone())),
        })
        .await
        .expect_err("staged refresh must recheck commit-time high-water");
    assert!(
        error
            .to_string()
            .contains("generation rollback or equivocation"),
        "{error}"
    );
    assert_eq!(f.snapshot(), before);
    concurrent.body.as_mut().expect("body").generation += 1;
    sign_set(&mut concurrent, 7);
    lookup(&device, Some(concurrent), &f.bundle, true);
    device
        .refresh_export_bundle(&session, &mut prepared)
        .await
        .expect("fresh N+2 control");
    let backend = device
        .hosted_backend(f.local(), session, prepared.clone())
        .expect("fresh selection");
    backend
        .receive(ReceivedOperation {
            native_authority: None,
            original: f.original.clone(),
            authority_admission: None,
            import_authority: Some(Arc::new(prepared)),
        })
        .await
        .expect("fresh control after concurrent durable advancement");
}
