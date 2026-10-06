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

use super::authority::{AcceptedHistory, PublicProof, SelectedAuthority};

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
