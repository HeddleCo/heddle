use std::cell::Cell;

use api::{
    heddle::api::v1alpha2 as wire,
    hybrid_codec::{self, Reject},
};
use prost::Message;
use repo::{
    Repository,
    thread_replication::{ThreadReplica, hosted_trust::*},
};

use super::authority::{AcceptedHistory, PublicEvidence, PublicProof, SelectedAuthority};

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/foreign-dependencies-alpha34.json"
    ))
    .expect("frozen mixed-origin corpus")
}
fn record<T: Message + Default>(name: &str) -> T {
    let f = fixture();
    hybrid_codec::strict_decode(
        &hex::decode(f["wire_vectors"][name]["wire_hex"].as_str().expect("wire")).expect("hex"),
        1048576,
    )
    .expect("wire record")
}
fn proof(name: &str, imported: bool) -> PublicProof {
    if imported {
        PublicProof::from(record::<wire::ImportPublicProofBundleV1>(name))
    } else {
        PublicProof::from(record::<wire::NativePublicProofBundleV1>(name))
    }
}
struct ReceiverClock;
impl Clock for ReceiverClock {
    fn now_millis(&self) -> repo::thread_replication::Result<i64> {
        Ok(1_200_001)
    }
    fn elapsed_millis(&self) -> repo::thread_replication::Result<u64> {
        Ok(0)
    }
}
struct Receiver {
    _directory: tempfile::TempDir,
    repo: Repository,
    trust: HostedTrust<ReceiverClock>,
}
impl Receiver {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("fresh receiver");
        let repo = Repository::init_default(directory.path()).expect("repository");
        let root = RootSelection {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: hex::decode(
                fixture()["keys"]["root"]["public_key_hex"]
                    .as_str()
                    .expect("root"),
            )
            .expect("hex")
            .try_into()
            .expect("root key"),
        };
        select_root(repo.heddle_dir(), &root).expect("independent root");
        let trust = HostedTrust::open(repo.heddle_dir(), &root.authority, ReceiverClock)
            .expect("receiver trust");
        Self {
            _directory: directory,
            repo,
            trust,
        }
    }
    fn install(
        &self,
        name: &str,
        imported: bool,
        originals: &[wire::SignedRecord],
    ) -> repo::thread_replication::Result<Vec<ThreadReplica>> {
        self.install_proof(proof(name, imported), originals)
    }
    fn install_proof(
        &self,
        bundle: PublicProof,
        originals: &[wire::SignedRecord],
    ) -> repo::thread_replication::Result<Vec<ThreadReplica>> {
        use super::authority::tests::{bundle as imported_fixture, selected};
        let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
            .expect("limits");
        let pinned = selected(&imported_fixture(), limits);
        let history = AcceptedHistory::from_public(&bundle, &pinned, 1200, limits)
            .expect("independent accepted history");
        select_spool(
            self.repo.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            *history.genesis(),
            *history.initial_owner(),
        )
        .expect("selected Spool");
        let authority = SelectedAuthority::from_proof(
            history,
            bundle.clone(),
            |_: &PublicProof, _: i64, _: &TrustTransaction<'_>| Ok(()),
        );
        match bundle {
            PublicProof::Import(b) => ThreadReplica::install_hybrid_import(
                self.repo.heddle_dir(),
                &self.trust,
                &b.encode_to_vec(),
                originals,
                &authority,
                self.repo.store(),
                |_| Ok(()),
            ),
            PublicProof::Native(b) => ThreadReplica::install_hybrid_native(
                self.repo.heddle_dir(),
                &self.trust,
                &b.encode_to_vec(),
                originals,
                &authority,
                self.repo.store(),
                |_| Ok(()),
            ),
        }
    }
    // Seed a retained journal fixture rather than repeatedly replaying each
    // growing closure during setup. The assertions below still enter the real
    // receiver, which authenticates and replays every retained signed prefix.
    fn retained_fixture(&self, prefixes: &[prefix_graph::Prefix]) {
        use rusqlite::params;
        self.stage("import_stage");
        self.install_proof(prefixes[0].proof.clone(), &prefixes[0].originals)
            .expect("public leaf control");
        let mut db = rusqlite::Connection::open(self.repo.heddle_dir().join("metadata.sqlite3"))
            .expect("journal");
        let tx = db.transaction().expect("fixture transaction");
        for prefix in prefixes {
            if let PublicProof::Native(b) = &prefix.proof {
                let p = &b.genesis_witnesses[0];
                let record = p.original_genesis.as_ref().expect("genesis");
                let (_, genesis) = crypto::import_authority::verify_native_genesis(record)
                    .expect("signed fixture genesis");
                let id = genesis.id().expect("thread");
                tx.execute("INSERT OR IGNORE INTO threads(id,genesis,genesis_signature,creator_authority) VALUES(?1,?2,?3,?4)",
                    params![id.as_bytes(),record.canonical_record,record.signatures[0].signature,p.creator_authority_envelope]).expect("genesis row");
                tx.execute("INSERT OR REPLACE INTO hosted_native_genesis_bindings(thread,binding) VALUES(?1,?2)",
                    params![id.as_bytes(),p.binding.as_ref().expect("binding").encode_to_vec()]).expect("binding row");
                tx.execute("INSERT OR REPLACE INTO hosted_native_proofs(thread,authority,bundle) VALUES(?1,?2,?3)",
                    params![id.as_bytes(),"https://weft.example.test",b.encode_to_vec()]).expect("native proof");
                let statement = b
                    .statements
                    .iter()
                    .find(|s| s.body.as_ref().expect("body").purpose == 1)
                    .expect("P1");
                tx.execute("INSERT OR REPLACE INTO hosted_import_admissions(operation,authority,statement) VALUES(?1,?2,?3)",params![id.as_bytes(),"https://weft.example.test",statement.encode_to_vec()]).expect("genesis admission row");
            } else if let PublicProof::Import(b) = &prefix.proof {
                let (_, operation) =
                    crypto::import_authority::verify_native_operation(&prefix.original)
                        .expect("operation");
                tx.execute("INSERT OR REPLACE INTO hosted_import_proofs(thread,authority,bundle) VALUES(?1,?2,?3)",
                    params![operation.thread.as_bytes(),"https://weft.example.test",b.encode_to_vec()]).expect("import proof");
            }
            for record in &prefix.originals {
                if record.format != objects::object::thread_replication::OPERATION_FORMAT {
                    continue;
                }
                let (_, operation) = crypto::import_authority::verify_native_operation(record)
                    .expect("signed fixture operation");
                let id = operation.id().expect("id");
                let revision = operation
                    .source_state()
                    .expect("source")
                    .map(|s| s.id().as_bytes().to_vec());
                tx.execute("INSERT OR IGNORE INTO operations(id,thread,facet,canonical,signature,status,source_revision,authority_admitted) VALUES(?1,?2,1,?3,?4,1,?5,1)",
                    params![id.as_bytes(),operation.thread.as_bytes(),record.canonical_record,record.signatures[0].signature,revision]).expect("operation row");
                for parent in &operation.parents {
                    tx.execute(
                        "INSERT OR IGNORE INTO parents(child,parent) VALUES(?1,?2)",
                        params![id.as_bytes(), parent.as_bytes()],
                    )
                    .expect("parent row");
                }
            }
            let authority = match &prefix.proof {
                PublicProof::Native(b) => &b.authority_witnesses,
                PublicProof::Import(b) => &b.authority_witnesses,
            };
            for payload in authority {
                let record = payload.original.as_ref().expect("original");
                let (_, operation) = crypto::import_authority::verify_native_operation(record)
                    .expect("signed operation");
                let id = operation.id().expect("id");
                let statement = prefix
                    .proof
                    .statements()
                    .iter()
                    .find(|s| {
                        s.body.as_ref().expect("body").canonical_payload
                            == hybrid_codec::canonical(payload).expect("payload")
                    })
                    .expect("P2");
                tx.execute("INSERT OR REPLACE INTO hosted_import_admissions(operation,authority,statement) VALUES(?1,?2,?3)",params![id.as_bytes(),"https://weft.example.test",statement.encode_to_vec()]).expect("admission row");
            }
        }
        tx.commit().expect("retained fixture");
    }
    fn stage(&self, name: &str) {
        let f = fixture();
        let descriptor = f["stages"]
            .as_array()
            .expect("stages")
            .iter()
            .find(|s| s["id"] == name)
            .expect("stage");
        let originals: Vec<_> = descriptor["originals"]
            .as_array()
            .expect("originals")
            .iter()
            .map(|s| record(s.as_str().expect("name")))
            .collect();
        self.install(name, descriptor["origin"] == 1, &originals)
            .unwrap_or_else(|e| panic!("stage {name}: {e:?}"));
    }
    fn snapshot(&self) -> Vec<(String, Vec<String>)> {
        super::native_tests::receiver_snapshot(self.repo.heddle_dir())
    }
}

#[test]
fn foreign_import_tip_lands_into_native_fast_forward_and_merge() {
    for name in ["import_tip_native_fast_forward", "import_tip_native_merge"] {
        let receiver = Receiver::new();
        receiver.stage("import_stage");
        let bundle: wire::NativePublicProofBundleV1 = record(name);
        let execution = bundle.landing_witnesses[0]
            .execution
            .clone()
            .expect("execution");
        receiver
            .install(name, false, &[execution])
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }
}
#[test]
fn foreign_native_child_lands_into_imported_main() {
    let receiver = Receiver::new();
    receiver.stage("native_child_stage");
    let bundle: wire::ImportPublicProofBundleV1 = record("native_child_imported_main");
    let execution = bundle.landing_witnesses[0]
        .execution
        .clone()
        .expect("execution");
    receiver
        .install(
            "native_child_imported_main",
            true,
            &[record("import_tip_0"), record("import_tip_1"), execution],
        )
        .expect("native child to imported main");
}
#[test]
fn missing_foreign_stage_rejects_inside_transaction_without_state_change() {
    let receiver = Receiver::new();
    let bundle: wire::NativePublicProofBundleV1 = record("import_tip_native_fast_forward");
    let published = Cell::new(false);
    use super::authority::tests::{bundle as imported, selected};
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let pinned = selected(&imported(), limits);
    let history =
        AcceptedHistory::from_native_spool(&bundle, &pinned, 1200, limits).expect("history");
    select_spool(
        receiver.repo.heddle_dir(),
        pinned.owner_genesis().spool_uuid(),
        *history.genesis(),
        *history.initial_owner(),
    )
    .expect("Spool");
    let before = receiver.snapshot();
    let authority = SelectedAuthority::new_native(
        history,
        bundle.clone(),
        |_: &wire::NativePublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let execution = bundle.landing_witnesses[0]
        .execution
        .clone()
        .expect("execution");
    let error = ThreadReplica::install_hybrid_native(
        receiver.repo.heddle_dir(),
        &receiver.trust,
        &bundle.encode_to_vec(),
        &[execution],
        &authority,
        receiver.repo.store(),
        |artifacts| {
            published.set(true);
            artifacts.write_file(std::path::Path::new("dependent"), b"must not appear")
        },
    )
    .err()
    .expect("missing stage");
    assert!(
        matches!(
            error,
            repo::thread_replication::Error::Hybrid(Reject::Scope)
        ),
        "{error:?}"
    );
    assert_eq!(receiver.snapshot(), before);
    assert!(!published.get());
    assert!(!receiver.repo.heddle_dir().join("dependent").exists());
    receiver.stage("import_stage");
    receiver
        .install(
            "import_tip_native_fast_forward",
            false,
            &[bundle.landing_witnesses[0]
                .execution
                .clone()
                .expect("execution")],
        )
        .expect("exact staged passing control");
}
#[test]
fn foreign_bidirectional_fresh_receiver_round_trip() {
    let stages = [
        "bidir_import_tip_prefix",
        "bidir_child_prefix",
        "bidir_main_landing_prefix",
        "bidir_child_sync_prefix",
        "bidir_main_complete_prefix",
    ];
    let first = Receiver::new();
    for name in stages {
        first.stage(name);
    }
    let second = Receiver::new();
    let f = fixture();
    for name in stages {
        let prefix = f["prefixes"]
            .as_array()
            .expect("prefixes")
            .iter()
            .find(|p| p["id"] == name)
            .expect("prefix");
        let reference = wire::ForeignDependencyV1 {
            format_version: 1,
            origin: if prefix["carrier"] == "import" { 1 } else { 2 },
            thread_genesis_digest: hex::decode(
                prefix["thread_genesis_digest"].as_str().expect("thread"),
            )
            .expect("hash"),
            signed_native_digest: hex::decode(
                prefix["signed_native_digest"].as_str().expect("original"),
            )
            .expect("hash"),
            prefix_admission_order: prefix["cutoff"]
                .as_str()
                .expect("cutoff")
                .parse()
                .expect("order"),
        };
        let replica = ThreadReplica::open(
            first.repo.heddle_dir(),
            objects::object::ContentHash::from_bytes(
                reference
                    .thread_genesis_digest
                    .as_slice()
                    .try_into()
                    .expect("thread"),
            ),
        )
        .expect("installed export");
        let exported = if reference.origin == 1 {
            PublicProof::from(
                replica
                    .hybrid_import_bundle()
                    .expect("retained import")
                    .expect("import origin"),
            )
        } else {
            PublicProof::from(
                replica
                    .hybrid_native_bundle()
                    .expect("retained native")
                    .expect("native origin"),
            )
        };
        let projected = exported
            .prefix(&reference)
            .unwrap_or_else(|e| panic!("prefix {name}: {e:?}"));
        assert!(
            projected
                .foreign_dependencies()
                .iter()
                .all(|r| exported.foreign_dependencies().contains(r)),
            "references retained verbatim"
        );
        let descriptor = f["stages"]
            .as_array()
            .expect("stages")
            .iter()
            .find(|s| s["id"] == name)
            .expect("stage");
        let originals: Vec<_> = descriptor["originals"]
            .as_array()
            .expect("originals")
            .iter()
            .map(|s| record(s.as_str().expect("name")))
            .collect();
        second
            .install_proof(projected, &originals)
            .unwrap_or_else(|e| panic!("re-export {name}: {e:?}"));
    }
}

#[test]
fn foreign_original_requires_a_durably_installed_operation() {
    let receiver = Receiver::new();
    receiver.stage("import_stage");
    let bundle: wire::NativePublicProofBundleV1 = record("import_tip_native_fast_forward");
    let source = bundle.landing_witnesses[0]
        .source_operation
        .clone()
        .expect("referenced source");
    let (_, op) =
        crypto::import_authority::verify_native_operation(&source).expect("signed source");
    let db = repo::local_metadata::open(receiver.repo.heddle_dir()).expect("metadata");
    db.execute(
        "UPDATE operations SET status=0 WHERE thread=?1 AND id=?2",
        [
            op.thread.as_bytes(),
            op.id().expect("operation ID").as_bytes(),
        ],
    )
    .expect("stage no longer installed");
    let before = receiver.snapshot();
    let bundle: wire::NativePublicProofBundleV1 = record("import_tip_native_fast_forward");
    let originals = [bundle.landing_witnesses[0]
        .execution
        .clone()
        .expect("execution")];
    let error = receiver
        .install("import_tip_native_fast_forward", false, &originals)
        .err()
        .expect("missing installed endpoint");
    assert!(
        matches!(
            error,
            repo::thread_replication::Error::Hybrid(Reject::Scope)
        ),
        "{error:?}"
    );
    assert_eq!(before, receiver.snapshot());
    db.execute(
        "DELETE FROM thread_source_heads WHERE thread=?1 AND operation=?2",
        [
            op.thread.as_bytes(),
            op.id().expect("operation ID").as_bytes(),
        ],
    )
    .expect("remove stale head before restoring admission");
    db.execute(
        "UPDATE operations SET status=1 WHERE thread=?1 AND id=?2",
        [
            op.thread.as_bytes(),
            op.id().expect("operation ID").as_bytes(),
        ],
    )
    .expect("restore exact installed endpoint");
    receiver
        .install("import_tip_native_fast_forward", false, &originals)
        .expect("same retained original and stage control");
}

#[test]
fn job_key_cannot_sign_landing_request() {
    let f = fixture();
    let name = f["negative"]
        .as_array()
        .expect("negatives")
        .iter()
        .find(|n| n["id"] == "job_signed_landing")
        .expect("job role guard");
    let denied: wire::NativePublicProofBundleV1 = record(name["id"].as_str().expect("name"));
    api::native_witness::validate_public_bundle(&denied)
        .expect("role needs receiver selected association");
    let receiver = Receiver::new();
    receiver.stage("import_stage");
    let error = receiver
        .install(
            "job_signed_landing",
            false,
            &[denied.landing_witnesses[0]
                .execution
                .clone()
                .expect("execution")],
        )
        .err()
        .expect("known job cannot request landing");
    assert!(
        matches!(
            error,
            repo::thread_replication::Error::Hybrid(Reject::KeyRole)
        ),
        "{error:?}"
    );
    let control: wire::NativePublicProofBundleV1 = record("import_tip_native_fast_forward");
    receiver
        .install(
            "import_tip_native_fast_forward",
            false,
            &[control.landing_witnesses[0]
                .execution
                .clone()
                .expect("execution")],
        )
        .expect("account request control");
}

#[path = "../../tests/support/foreign_prefix.rs"]
mod prefix_graph;

#[test]
fn foreign_prefix_replay_depth_33_refuses_with_typed_limit() {
    // Retained verification recurses through signed proof prefixes in debug
    // builds. Use the CLI runtime's stack allowance for this boundary fixture.
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(replay_depth_33)
        .expect("replay thread")
        .join()
        .expect("depth assertion");
}

fn replay_depth_33() {
    use repo::thread_replication::Error;
    let receiver = Receiver::new();
    let chain = prefix_graph::chain(33);
    receiver.retained_fixture(&chain[..32]);
    receiver
        .install_proof(chain[32].proof.clone(), &chain[32].originals)
        .expect("depth 32 control installs through the real receiver");
    let bundle = chain.last().expect("depth 33");
    let staged = stage_prefix(bundle);
    std::fs::write(
        receiver.repo.heddle_dir().join("spool-id"),
        &staged
            .ready()
            .thread
            .as_ref()
            .expect("Thread")
            .spool
            .as_ref()
            .expect("Spool")
            .id,
    )
    .expect("selected fixture Spool");
    let before = receiver.snapshot();
    let limits =
        heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let pinned = super::authority::tests::selected(&super::authority::tests::bundle(), limits);
    let history =
        AcceptedHistory::from_public(&bundle.proof, &pinned, 1200, limits).expect("history");
    let authority = SelectedAuthority::from_proof(
        history,
        bundle.proof.clone(),
        |_: &PublicProof, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let result = staged.install_hosted(&receiver.repo, &receiver.trust, &authority, 1200);
    assert!(
        matches!(&result, Err(crate::fetch::Error::Repository(error))
        if matches!(**error, Error::ForeignPrefixLimitExceeded { limit_name: "depth", limit: 32 })),
        "hosted replay must preserve the repository's typed depth refusal: {result:?}"
    );
    assert_eq!(receiver.snapshot(), before);
}

#[test]
fn installed_foreign_closure_larger_than_fetch_count_remains_installable() {
    let receiver = Receiver::new();
    // 257 leaves, three branch prefixes and a root. Each ordinary signed
    // bundle stays below the wire's per-record dependency bounds.
    let mut edges = vec![vec![]; 257];
    for start in [0, 86, 172] {
        edges.push((start..(start + 86).min(257)).collect());
    }
    edges.push(vec![257, 258, 259]);
    let graph = prefix_graph::graph(&edges);
    receiver.retained_fixture(&graph[..260]);
    let root = &graph[260];
    receiver
        .install_proof(root.proof.clone(), &root.originals)
        .expect("installed closure spends no Fetch count");
}

fn stage_prefix(prefix: &prefix_graph::Prefix) -> crate::fetch::StagedSource {
    use crate::{contract::*, publication::*};
    let source = prefix_graph::source(prefix);
    let directory = tempfile::tempdir().expect("staging");
    std::fs::write(directory.path().join("source.pack"), &source.pack).expect("pack");
    std::fs::write(directory.path().join("source.idx"), &source.index).expect("index");
    let (_, operation) =
        crypto::import_authority::verify_native_operation(&prefix.original).expect("original");
    let spool = SpoolRef {
        id: uuid::Uuid::from_slice(
            &source
                .owner_genesis
                .genesis
                .as_ref()
                .expect("Spool")
                .spool_uuid,
        )
        .expect("UUID")
        .to_string(),
    };
    let thread = ThreadRef {
        spool: Some(spool.clone()),
        id: Some(ThreadId {
            value: operation.thread.as_bytes().to_vec(),
        }),
    };
    let revision = RevisionRef {
        spool: Some(spool),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::common::StateId {
                value: source.state.id().as_bytes().to_vec(),
            },
        )),
    };
    let (imported, native) = match &prefix.proof {
        PublicProof::Import(b) => (Some(*b.clone()), None),
        PublicProof::Native(b) => (None, Some(*b.clone())),
    };
    let packs = [&source.pack, &source.index]
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let address = ObjectAddress {
                algorithm: "blake3".into(),
                digest: objects::object::ContentHash::compute(bytes)
                    .as_bytes()
                    .to_vec(),
            };
            PackExtent {
                pack: Some(address.clone()),
                extent_digest: Some(address),
                length: bytes.len() as u64,
                kind: if i == 0 {
                    pack_extent::Kind::NativePack as i32
                } else {
                    pack_extent::Kind::NativeIndex as i32
                },
                ..Default::default()
            }
        })
        .collect();
    let opening = PublishContentOpen {
        protocol: Some(crate::hybrid::protocol()),
        thread: Some(thread.clone()),
        revision: Some(revision.clone()),
        packs,
        import_authority: imported.clone(),
        native_authority: native.clone(),
        ..Default::default()
    };
    let originals = PublicationOriginals {
        geneses: vec![source.genesis],
        operations: vec![ReplicationOperations {
            operations: source.operations,
            import_authority: imported.clone(),
            native_authority: native.clone(),
            ..Default::default()
        }],
    };
    let artifacts = if let Some(b) = &imported {
        let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
            .expect("limits");
        let pinned = super::authority::tests::selected(&super::authority::tests::bundle(), limits);
        let history =
            AcceptedHistory::from_public(&prefix.proof, &pinned, 1200, limits).expect("history");
        let authority = SelectedAuthority::from_proof(
            history,
            prefix.proof.clone(),
            |_: &PublicProof, _: i64, _: &TrustTransaction<'_>| Ok(()),
        );
        let f = fixture();
        let pin = api::import_authority::ImportWitnessRootPin {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: hex::decode(f["keys"]["root"]["public_key_hex"].as_str().expect("root"))
                .expect("hex"),
            epoch: 1,
        };
        let carriers = repo::thread_replication::delegated_import::authenticate_import_carriers(
            b,
            &authority,
            &pin,
            1_200_001,
            &[],
            &[],
            |_| Ok(()),
        )
        .expect("import carriers");
        validate_source_artifacts_with_import_carriers(directory, &opening, originals, carriers)
    } else {
        validate_source_artifacts(directory, &opening, originals)
    }
    .expect("signed source staging");
    artifacts
        .into_hosted_source(TransferReady {
            protocol: Some(crate::hybrid::protocol()),
            thread: Some(thread),
            current: Some(revision),
            owner_genesis: Some(source.owner_genesis),
            ownership: Some(source.owner),
            import_authority: imported,
            native_authority: native,
            full_closure_available: true,
            ..Default::default()
        })
        .expect("hosted staged source")
}
