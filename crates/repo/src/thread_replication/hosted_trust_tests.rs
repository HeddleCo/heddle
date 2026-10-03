use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
};

use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec, witness_trust,
};
use crypto::{Ed25519Signer, Signer};
use prost::Message;
use serde_json::Value;

use super::{Error, Result, ThreadReplica, hosted_trust::*};
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/hybrid/import-authority-host-witness-v1.json"
    ))
    .expect("api fixed fixture")
}
fn record<T: Message + Default>(f: &Value, name: &str) -> T {
    let v = f["signed_vectors"]
        .get(name)
        .or_else(|| f["wire_vectors"].get(name))
        .expect("vector");
    hybrid_codec::strict_decode(
        &hex::decode(v["wire_hex"].as_str().expect("hex")).expect("bytes"),
        api::import_authority::MAX_BUNDLE_BYTES,
    )
    .expect("canonical fixed record")
}
fn key(f: &Value, name: &str) -> [u8; 32] {
    hex::decode(f["keys"][name]["public_key_hex"].as_str().expect("key"))
        .expect("bytes")
        .try_into()
        .expect("key length")
}
fn root(f: &Value) -> RootSelection {
    RootSelection {
        authority: "https://weft.example.test".into(),
        root_id: "descriptor-root-1".into(),
        public_key: key(f, "root"),
    }
}
#[derive(Clone)]
struct TestClock {
    wall: Arc<AtomicI64>,
    elapsed: Arc<AtomicU64>,
}
impl TestClock {
    fn new(now: i64) -> Self {
        Self {
            wall: Arc::new(AtomicI64::new(now)),
            elapsed: Arc::new(AtomicU64::new(0)),
        }
    }
    fn set(&self, now: i64) {
        self.wall.store(now, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_millis(&self) -> Result<i64> {
        let t = self.wall.load(Ordering::SeqCst);
        if t < 0 {
            Err(Error::HostedClock)
        } else {
            Ok(t)
        }
    }
    fn elapsed_millis(&self) -> Result<u64> {
        Ok(self.elapsed.load(Ordering::SeqCst))
    }
}
fn resign(f: &Value, set: &mut host::SignedHostedWitnessSetV1) {
    let seed = hex::decode(f["keys"]["root"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    set.body_digest =
        hybrid_codec::hash(&[
            &witness_trust::set_signing_bytes(set.body.as_ref().expect("body")).expect("bytes"),
        ]);
    set.root_signature = Ed25519Signer::from_seed(&seed)
        .expect("signer")
        .sign(
            &witness_trust::set_signing_bytes(set.body.as_ref().expect("body"))
                .expect("signing bytes"),
        )
        .expect("malformed input signature");
}
#[test]
fn atomic_mutation_high_water_restart_and_clock_floor() {
    let f = fixture();
    let dir = tempfile::tempdir().expect("directory");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let selected = root(&f);
    assert!(matches!(
        HostedTrust::open(
            repo.heddle_dir(),
            &selected.authority,
            TestClock::new(1100000)
        ),
        Err(Error::Hybrid(hybrid_codec::Reject::Root))
    ));
    select_root(repo.heddle_dir(), &selected).expect("independent root");
    let clock = TestClock::new(1100000);
    let trust =
        HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone()).expect("trust");
    let current = record(&f, "current_set");
    trust.mutate(&current, |_| Ok(())).expect("current control");
    let mut newer: host::SignedHostedWitnessSetV1 = record(&f, "newer_set");
    clock.set(1100000);
    let before = crate::local_metadata::open(repo.heddle_dir())
        .expect("db")
        .query_row("SELECT signed_set FROM hosted_witness_trust", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .expect("snapshot");
    assert!(matches!(
        trust.mutate(&newer, |c| {
            c.sql().execute(
                "INSERT INTO hosted_import_job_keys VALUES(?1,?2)",
                rusqlite::params![[99u8; 32], [99u8; 16]],
            )?;
            Err::<(), _>(Error::Hybrid(hybrid_codec::Reject::Scope))
        }),
        Err(Error::Hybrid(hybrid_codec::Reject::Scope))
    ));
    let db = crate::local_metadata::open(repo.heddle_dir()).expect("db");
    assert_eq!(
        db.query_row("SELECT count(*) FROM hosted_import_job_keys", [], |r| r
            .get::<_, i64>(0))
            .expect("count"),
        0
    );
    assert_eq!(
        db.query_row("SELECT signed_set FROM hosted_witness_trust", [], |r| r
            .get::<_, Vec<u8>>(0))
            .expect("snapshot"),
        before
    );
    trust.mutate(&newer, |_| Ok(())).expect("N+1 control");
    let reopen =
        HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone()).expect("restart");
    assert!(matches!(
        reopen.mutate(&current, |_| Ok(())),
        Err(Error::Hybrid(hybrid_codec::Reject::HighWater))
    ));
    newer.body.as_mut().expect("body").valid_until_unix_millis += 1;
    resign(&f, &mut newer);
    assert!(matches!(
        reopen.mutate(&newer, |_| Ok(())),
        Err(Error::Hybrid(hybrid_codec::Reject::HighWater))
    ));
    clock.set(1000000);
    assert!(matches!(
        reopen.mutate(&current, |_| Ok(())),
        Err(Error::HostedClock)
    ));
    clock.set(-1);
    assert!(matches!(
        reopen.mutate(&current, |_| Ok(())),
        Err(Error::HostedClock)
    ));
}
#[test]
fn cached_context_concurrent_revocation_and_root_replacement() {
    let f = fixture();
    let dir = tempfile::tempdir().expect("dir");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let selected = root(&f);
    select_root(repo.heddle_dir(), &selected).expect("root");
    let clock = TestClock::new(1350000);
    let trust =
        HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone()).expect("trust");
    let retired = record(&f, "retired_set");
    let s = record(&f, "publication_statement");
    let proof = record(&f, "publication_proof");
    let (set, cached) = trust
        .mutate(&retired, |c| {
            Ok((
                c.set().clone(),
                witness_trust::resolve_statement(c.set(), &s, Some(&proof), false, c.now_millis())?,
            ))
        })
        .expect("retired control");
    let second = HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone())
        .expect("independent transfer handle");
    let revoked = record(&f, "revoked_set");
    std::thread::spawn(move || second.mutate(&revoked, |_| Ok(())))
        .join()
        .expect("writer")
        .expect("revocation commit");
    assert!(matches!(
        trust.mutate(&retired, |_| Ok(())),
        Err(Error::Hybrid(hybrid_codec::Reject::HighWater))
    ));
    let revoked = record(&f, "revoked_set");
    assert!(matches!(
        trust.mutate(&revoked, |c| {
            witness_trust::recheck_context(&cached, c.set(), &s, c.now_millis())?;
            Ok(())
        }),
        Err(Error::Hybrid(hybrid_codec::Reject::StaleContext))
    ));
    let replacement = RootSelection {
        public_key: key(&f, "wrong_root"),
        ..selected.clone()
    };
    assert!(matches!(
        select_root(repo.heddle_dir(), &replacement),
        Err(Error::Hybrid(hybrid_codec::Reject::Root))
    ));
    replace_root(repo.heddle_dir(), &selected, &replacement).expect("explicit routine replacement");
    let mut fresh: host::SignedHostedWitnessSetV1 = record(&f, "revoked_set");
    let seed =
        hex::decode(f["keys"]["wrong_root"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    fresh.root_signature = Ed25519Signer::from_seed(&seed)
        .expect("signer")
        .sign(
            &witness_trust::set_signing_bytes(fresh.body.as_ref().expect("body"))
                .expect("preimage"),
        )
        .expect("new root signature");
    trust
        .mutate(&fresh, |c| {
            assert_ne!(set.root_epoch(), c.set().root_epoch());
            assert_eq!(
                witness_trust::recheck_context(&cached, c.set(), &s, c.now_millis()),
                Err(hybrid_codec::Reject::StaleContext)
            );
            Ok(())
        })
        .expect("known tombstones survive new root");
    let mut enlarged = fresh.clone();
    enlarged.body.as_mut().expect("body").generation += 1;
    let entry = enlarged
        .body
        .as_mut()
        .expect("body")
        .entries
        .iter_mut()
        .find(|e| e.state == 3)
        .expect("tombstone");
    entry.archive_root = [22; 32].to_vec();
    entry.archive_leaf_count += 1;
    enlarged.body_digest = hybrid_codec::hash(&[&witness_trust::set_signing_bytes(
        enlarged.body.as_ref().expect("body"),
    )
    .expect("bytes")]);
    enlarged.root_signature = Ed25519Signer::from_seed(&seed)
        .expect("root")
        .sign(
            &witness_trust::set_signing_bytes(enlarged.body.as_ref().expect("body"))
                .expect("preimage"),
        )
        .expect("signature");
    assert!(matches!(
        trust.mutate(&enlarged, |_| Ok(())),
        Err(Error::Hybrid(hybrid_codec::Reject::Transition))
    ));
}
struct Authority {
    disclosure: AtomicBool,
    owner: heddleco_capability_verifier::VerifiedOwnerState,
    ring: heddleco_capability_verifier::VerifiedCloneKeyring,
    digest: [u8; 32],
    initial: [u8; 32],
}
impl Authority {
    fn new(f: &Value) -> Self {
        use heddleco_capability_verifier as v;
        let h: wire::OwnerHistory = record(f, "owner_history");
        let owner = v::verify_owner_root(h.root.as_ref().expect("root")).expect("owner");
        let genesis: wire::SignedSpoolOwnerGenesis = record(f, "spool_owner_genesis");
        let digest = v::creation::spool_genesis_digest(genesis.genesis.as_ref().expect("body"))
            .expect("digest");
        let initial = owner.owner_id();
        let ring = v::verify_clone_keyring(
            wire::CloneAuthorizationKeyring {
                format_version: 1,
                spool_uuid: genesis.genesis.as_ref().expect("body").spool_uuid.clone(),
                owner_genesis: Some(genesis),
                owner_root: h.root,
                canonical_spool_path_segments: vec!["example".into()],
                accepted_state_hash: owner.state_hash().to_vec(),
                pin: Some(wire::CloneOwnerPin {
                    kind: 2,
                    expected_owner_id: initial.to_vec(),
                    first_seen_unix_seconds: 1000,
                }),
                ..Default::default()
            },
            1100,
            v::VerificationLimits::new(3600).expect("limits"),
            &[],
        )
        .expect("keyring");
        Self {
            disclosure: AtomicBool::new(true),
            owner,
            ring,
            digest,
            initial,
        }
    }
    fn selection(&self) -> heddleco_capability_verifier::import_delegation::Selection<'_> {
        heddleco_capability_verifier::import_delegation::Selection {
            owner: &self.owner,
            keyring: &self.ring,
            spool_genesis_digest: &self.digest,
            initial_owner_id: &self.initial,
            limits: heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits"),
        }
    }
}
impl super::delegated_import::AcceptedAuthority for Authority {
    fn authorize_import(&self, _: &wire::ImportPublicProofBundleV1, now_millis: i64) -> Result<()> {
        assert_eq!(
            now_millis, 1350000,
            "receiver time, not claimed author time"
        );
        if self.disclosure.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::Hybrid(hybrid_codec::Reject::Revoked))
        }
    }
    fn for_witness(
        &self,
        _: &host::HostedWitnessStatementV1,
    ) -> Result<heddleco_capability_verifier::import_delegation::Selection<'_>> {
        Ok(self.selection())
    }
    fn for_policy(
        &self,
        _: &wire::SignedPolicyBody,
    ) -> Result<heddleco_capability_verifier::import_delegation::Selection<'_>> {
        Ok(self.selection())
    }
    fn import_revoked(
        &self,
        _: &host::HostedWitnessStatementV1,
        _: heddleco_capability_verifier::import_delegation::Revocation<'_>,
    ) -> bool {
        false
    }
    fn native_revoked(
        &self,
        _: &host::HostedWitnessStatementV1,
        _: heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool {
        false
    }
}
#[test]
fn complete_renewed_import_is_atomic_exact_and_does_not_enroll_foreign_owner() {
    let f = fixture();
    let dir = tempfile::tempdir().expect("dir");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let head = repo.head().expect("head");
    let selected = root(&f);
    select_root(repo.heddle_dir(), &selected).expect("root");
    let a = Authority::new(&f);
    select_spool(
        repo.heddle_dir(),
        a.ring.owner_genesis().spool_uuid(),
        a.digest,
        a.initial,
    )
    .expect("independent spool");
    let trust = HostedTrust::open(
        repo.heddle_dir(),
        &selected.authority,
        TestClock::new(1350000),
    )
    .expect("trust");
    let mut bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    bundle.history_proofs = [
        "genesis_proof",
        "genesis_dev_proof",
        "publication_proof",
        "renewed_publication_proof",
    ]
    .iter()
    .map(|n| record(&f, n))
    .collect();
    let records = [record(&f, "converted_main"), record(&f, "converted_dev")];
    let replicas = ThreadReplica::install_hybrid_import(
        repo.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &records,
        &a,
        repo.store(),
    )
    .expect("full renewed import");
    assert_eq!(replicas.len(), 2);
    assert_eq!(repo.head().expect("head"), head);
    for r in &replicas {
        assert_eq!(
            r.hybrid_import_bundle().expect("retained proofs"),
            Some(bundle.clone())
        );
        let admission = r
            .hosted_admission(r.thread)
            .expect("genesis sidecar")
            .expect("original genesis witness");
        assert!(bundle.statements.contains(&admission.statement));
        assert!(admission.proof.is_some());
    }
    ThreadReplica::install_hybrid_import(
        repo.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &records,
        &a,
        repo.store(),
    )
    .expect("exact replay");
    let db = crate::local_metadata::open(repo.heddle_dir()).expect("metadata");
    assert_eq!(
        db.query_row("SELECT count(*) FROM hosted_import_job_keys", [], |r| r
            .get::<_, i64>(0))
            .expect("jobs"),
        2
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM hosted_import_slots", [], |r| r
            .get::<_, i64>(0))
            .expect("slots"),
        2
    );
    assert!(!repo.heddle_dir().join("owner-authorization.bin").exists());
    let job_root = RootSelection {
        public_key: key(&f, "job"),
        ..selected.clone()
    };
    assert!(matches!(
        select_root(repo.heddle_dir(), &job_root),
        Err(Error::Hybrid(hybrid_codec::Reject::KeyRole))
    ));
    assert!(matches!(
        replace_root(repo.heddle_dir(), &selected, &job_root),
        Err(Error::Hybrid(hybrid_codec::Reject::KeyRole))
    ));
    assert_eq!(
        db.query_row("SELECT root_key FROM hosted_witness_trust", [], |r| r
            .get::<_, Vec<u8>>(0))
            .expect("unchanged selected root"),
        selected.public_key
    );
    a.disclosure.store(false, Ordering::SeqCst);
    assert!(matches!(
        ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &a,
            repo.store()
        ),
        Err(Error::Hybrid(hybrid_codec::Reject::Revoked))
    ));
    a.disclosure.store(true, Ordering::SeqCst);
    ThreadReplica::install_hybrid_import(
        repo.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &records,
        &a,
        repo.store(),
    )
    .expect("restored current disclosure control");
    let mut missing = bundle.clone();
    missing.history_proofs.clear();
    assert!(matches!(
        ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &missing.encode_to_vec(),
            &records,
            &a,
            repo.store()
        ),
        Err(Error::Hybrid(hybrid_codec::Reject::Proof))
    ));
    let before = replicas
        .iter()
        .map(|r| r.view().expect("view"))
        .collect::<Vec<_>>();
    let mut revoked = bundle.clone();
    revoked.witness_set = Some(record(&f, "revoked_set"));
    trust
        .mutate(revoked.witness_set.as_ref().expect("set"), |_| Ok(()))
        .expect("learn revocation");
    assert!(matches!(
        ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &revoked.encode_to_vec(),
            &records,
            &a,
            repo.store()
        ),
        Err(Error::HybridEvidence(
            crypto::import_authority::Error::Contract(hybrid_codec::Reject::Revoked)
        ))
    ));
    for (r, b) in replicas.iter().zip(before) {
        assert_eq!(r.view().expect("unchanged").generation, b.generation);
    }
}
#[test]
fn set_expiry_during_mutation_and_monotonic_rollback_leave_no_durable_authority() {
    let f = fixture();
    let dir = tempfile::tempdir().expect("dir");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let selected = root(&f);
    select_root(repo.heddle_dir(), &selected).expect("root");
    let clock = TestClock::new(1100000);
    let trust =
        HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone()).expect("trust");
    let current: host::SignedHostedWitnessSetV1 = record(&f, "current_set");
    trust.mutate(&current, |_| Ok(())).expect("fresh control");
    let end = current.body.as_ref().expect("body").valid_until_unix_millis;
    assert!(matches!(
        trust.mutate(&current, |c| {
            c.sql().execute(
                "INSERT INTO hosted_import_job_keys VALUES(?1,?2)",
                rusqlite::params![[98u8; 32], [98u8; 16]],
            )?;
            clock.set(end);
            Ok(())
        }),
        Err(Error::Hybrid(hybrid_codec::Reject::Expired))
    ));
    assert_eq!(
        crate::local_metadata::open(repo.heddle_dir())
            .expect("db")
            .query_row("SELECT count(*) FROM hosted_import_job_keys", [], |r| r
                .get::<_, i64>(0))
            .expect("count"),
        0
    );
    clock.set(1100000);
    clock.elapsed.store(100, Ordering::SeqCst);
    assert!(matches!(
        trust.mutate(&current, |_| Ok(())),
        Err(Error::HostedClock)
    ));
    clock.set(1100100);
    trust
        .mutate(&current, |_| Ok(()))
        .expect("restored trustworthy receiver time control");
    clock.set(1100199);
    clock.elapsed.store(200, Ordering::SeqCst);
    trust
        .mutate(&current, |_| Ok(()))
        .expect("independent millisecond truncation control");
    assert!(matches!(
        trust.mutate(&current, |_| {
            clock.elapsed.store(300, Ordering::SeqCst);
            Ok(())
        }),
        Err(Error::HostedClock)
    ));
}
#[test]
fn witnessed_native_control_commits_its_exact_original_and_invalidates_replay() {
    use super::delegated_import::{NativeEvidence, NativeSubject};
    let f = fixture();
    let dir = tempfile::tempdir().expect("dir");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let selected = root(&f);
    select_root(repo.heddle_dir(), &selected).expect("root");
    let a = Authority::new(&f);
    select_spool(
        repo.heddle_dir(),
        a.ring.owner_genesis().spool_uuid(),
        a.digest,
        a.initial,
    )
    .expect("selected Spool");
    let trust = HostedTrust::open(
        repo.heddle_dir(),
        &selected.authority,
        TestClock::new(1350000),
    )
    .expect("trust");
    let mut bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    bundle.history_proofs = [
        "genesis_proof",
        "genesis_dev_proof",
        "publication_proof",
        "renewed_publication_proof",
    ]
    .iter()
    .map(|n| record(&f, n))
    .collect();
    let conversions = [record(&f, "converted_main"), record(&f, "converted_dev")];
    ThreadReplica::install_hybrid_import(
        repo.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &conversions,
        &a,
        repo.store(),
    )
    .expect("import original genesis controls");
    let payload: wire::ImportAuthorityWitnessV1 = record(&f, "authority_admission_payload");
    let (_, op) = crypto::import_authority::verify_native_operation(
        payload.original.as_ref().expect("original"),
    )
    .expect("native original");
    let replica =
        ThreadReplica::open(repo.heddle_dir(), op.thread).expect("existing imported Thread");
    let set = record(&f, "retired_set");
    let statement = record(&f, "authority_admission");
    let proof = record(&f, "authority_proof");
    let policy = record(&f, "signed_policy");
    let mut originals = bundle.original_geneses;
    originals.extend(conversions);
    originals.extend(payload.original.iter().cloned());
    originals.extend(payload.dependencies.clone());
    let input = NativeEvidence {
        set: &set,
        statement: &statement,
        proof: Some(&proof),
        policy: &policy,
        originals: &originals,
        genesis_witnesses: &bundle.genesis_witnesses,
        subject: NativeSubject::Authority(&payload),
    };
    assert_eq!(
        replica
            .receive_witnessed(&trust, &input, &a, repo.store(), |_, _| Ok(()))
            .expect("independent native control and exact witness"),
        objects::object::thread_replication::Admission::Accepted
    );
    let retained = replica
        .hosted_admission(op.id().expect("id"))
        .expect("retained sidecar")
        .expect("witness");
    assert_eq!(retained.statement, statement);
    assert_eq!(retained.proof, Some(proof.clone()));
    assert_eq!(retained.deployment_authority, selected.authority);
    assert!(matches!(
        replica.receive(
            &crypto::import_authority::verify_native_operation(
                payload.original.as_ref().expect("original")
            )
            .expect("native")
            .0,
            repo.store(),
            |_| Ok(())
        ),
        Err(Error::WitnessEvidenceRequired)
    ));
    let revoked = record(&f, "revoked_set");
    trust
        .mutate(&revoked, |_| Ok(()))
        .expect("learned revocation");
    let revoked = NativeEvidence {
        set: &revoked,
        ..input
    };
    assert!(matches!(
        replica.receive_witnessed(&trust, &revoked, &a, repo.store(), |_, _| Ok(())),
        Err(Error::HybridEvidence(
            crypto::import_authority::Error::Contract(hybrid_codec::Reject::Revoked)
        ))
    ));
}
#[test]
fn incomplete_public_authority_and_conflicting_job_keys_are_not_cached_grants() {
    let f = fixture();
    let dir = tempfile::tempdir().expect("dir");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let selected = root(&f);
    select_root(repo.heddle_dir(), &selected).expect("root");
    let a = Authority::new(&f);
    select_spool(
        repo.heddle_dir(),
        a.ring.owner_genesis().spool_uuid(),
        a.digest,
        a.initial,
    )
    .expect("independent Spool");
    let trust = HostedTrust::open(
        repo.heddle_dir(),
        &selected.authority,
        TestClock::new(1350000),
    )
    .expect("trust");
    let mut bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    bundle.history_proofs = [
        "genesis_proof",
        "genesis_dev_proof",
        "publication_proof",
        "renewed_publication_proof",
    ]
    .iter()
    .map(|n| record(&f, n))
    .collect();
    let records = [record(&f, "converted_main"), record(&f, "converted_dev")];
    let mut bad = bundle.clone();
    bad.owner_histories[0]
        .root
        .as_mut()
        .expect("root")
        .authority_proof
        .as_mut()
        .expect("signature")
        .signature[0] ^= 1;
    assert!(matches!(
        ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &bad.encode_to_vec(),
            &records,
            &a,
            repo.store()
        ),
        Err(Error::ImportAuthority(
            heddleco_capability_verifier::Error::InvalidSignature
        ))
    ));
    trust
        .mutate(bundle.witness_set.as_ref().expect("set"), |c| {
            c.sql().execute(
                "INSERT INTO hosted_import_job_keys VALUES(?1,?2)",
                rusqlite::params![key(&f, "job"), [89u8; 16]],
            )?;
            Ok(())
        })
        .expect("other previously verified job association");
    assert!(matches!(
        ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &a,
            repo.store()
        ),
        Err(Error::ImportAuthority(
            heddleco_capability_verifier::Error::Hybrid(hybrid_codec::Reject::Scope)
        ))
    ));
    let db = crate::local_metadata::open(repo.heddle_dir()).expect("db");
    assert_eq!(
        db.query_row("SELECT count(*) FROM hosted_import_proofs", [], |r| r
            .get::<_, i64>(0))
            .expect("proof count"),
        0
    );
    db.execute(
        "DELETE FROM hosted_import_job_keys WHERE public_key=?1",
        [key(&f, "job")],
    )
    .expect("remove test-only conflicting association");
    ThreadReplica::install_hybrid_import(
        repo.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &records,
        &a,
        repo.store(),
    )
    .expect("unchanged complete positive control");
}

#[test]
fn review_clock_failure_survives_independent_handles_and_reopen() {
    let f = fixture();
    let dir = tempfile::tempdir().expect("dir");
    let repo = crate::Repository::init_default(dir.path()).expect("repo");
    let selected = root(&f);
    select_root(repo.heddle_dir(), &selected).expect("root");
    let clock = TestClock::new(1350000);
    let trust =
        HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone()).expect("trust");
    let independent = HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone())
        .expect("independent");
    let set = record(&f, "retired_set");
    trust.mutate(&set, |_| Ok(())).expect("fresh control");
    clock.elapsed.store(600000, Ordering::SeqCst);
    assert!(matches!(
        trust.mutate(&set, |_| Ok(())),
        Err(Error::HostedClock)
    ));
    assert!(
        matches!(
            independent.mutate(&set, |_| Ok(())),
            Err(Error::HostedClock)
        ),
        "independent open retains clock guard"
    );
    drop(trust);
    drop(independent);
    let reopened =
        HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone()).expect("reopen");
    assert!(
        matches!(reopened.mutate(&set, |_| Ok(())), Err(Error::HostedClock)),
        "reopen after rejection retains failure"
    );
    clock.set(1950000);
    let mut fresh: host::SignedHostedWitnessSetV1 = record(&f, "retired_set");
    fresh.body.as_mut().expect("body").generation += 1;
    fresh.body.as_mut().expect("body").issued_at_unix_millis = 1950000;
    fresh.body.as_mut().expect("body").valid_until_unix_millis = 2250000;
    fresh
        .body
        .as_mut()
        .expect("body")
        .entries
        .iter_mut()
        .find(|e| e.state == 1)
        .expect("current witness")
        .active_until_unix_millis = 2250000;
    resign(&f, &mut fresh);
    reopened
        .mutate(&fresh, |_| Ok(()))
        .expect("restored trustworthy time control");
}

struct ExpiringDisclosure {
    authority: Authority,
    clock: TestClock,
    advance: bool,
    calls: AtomicU64,
}
impl super::delegated_import::AcceptedAuthority for ExpiringDisclosure {
    fn authorize_import(&self, _: &wire::ImportPublicProofBundleV1, now: i64) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if now >= 1351000 {
            return Err(Error::Hybrid(hybrid_codec::Reject::Expired));
        }
        if self.advance {
            self.clock.set(1352000);
        }
        Ok(())
    }
    fn for_witness(
        &self,
        s: &host::HostedWitnessStatementV1,
    ) -> Result<heddleco_capability_verifier::import_delegation::Selection<'_>> {
        super::delegated_import::AcceptedAuthority::for_witness(&self.authority, s)
    }
    fn for_policy(
        &self,
        p: &wire::SignedPolicyBody,
    ) -> Result<heddleco_capability_verifier::import_delegation::Selection<'_>> {
        super::delegated_import::AcceptedAuthority::for_policy(&self.authority, p)
    }
    fn import_revoked(
        &self,
        _: &host::HostedWitnessStatementV1,
        _: heddleco_capability_verifier::import_delegation::Revocation<'_>,
    ) -> bool {
        false
    }
    fn native_revoked(
        &self,
        _: &host::HostedWitnessStatementV1,
        _: heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool {
        false
    }
}
#[test]
fn review_disclosure_expiry_is_checked_at_commit_without_durable_changes() {
    let f = fixture();
    for (now, advance, accepted) in [
        (1350000, false, true),
        (1352000, false, false),
        (1350000, true, false),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let repo = crate::Repository::init_default(dir.path()).expect("repo");
        let selected = root(&f);
        select_root(repo.heddle_dir(), &selected).expect("root");
        let clock = TestClock::new(now);
        let a = ExpiringDisclosure {
            authority: Authority::new(&f),
            clock: clock.clone(),
            advance,
            calls: AtomicU64::new(0),
        };
        select_spool(
            repo.heddle_dir(),
            a.authority.ring.owner_genesis().spool_uuid(),
            a.authority.digest,
            a.authority.initial,
        )
        .expect("Spool pin");
        let trust =
            HostedTrust::open(repo.heddle_dir(), &selected.authority, clock).expect("trust");
        let mut bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
        bundle.history_proofs = [
            "genesis_proof",
            "genesis_dev_proof",
            "publication_proof",
            "renewed_publication_proof",
        ]
        .iter()
        .map(|n| record(&f, n))
        .collect();
        let records = [record(&f, "converted_main"), record(&f, "converted_dev")];
        let db = crate::local_metadata::open(repo.heddle_dir()).expect("db");
        let before: i64 = db
            .query_row("SELECT count(*) FROM threads", [], |r| r.get(0))
            .expect("threads");
        let result = ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &a,
            repo.store(),
        );
        if accepted {
            assert_eq!(result.expect("unexpired control").len(), 2);
        } else {
            assert!(
                matches!(result, Err(Error::Hybrid(hybrid_codec::Reject::Expired))),
                "deadline must reject"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
                    .expect("threads"),
                before
            );
            for table in [
                "hosted_import_job_keys",
                "hosted_import_proofs",
                "hosted_import_admissions",
                "hosted_import_slots",
            ] {
                assert_eq!(
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .expect("count"),
                    0
                );
            }
            assert_eq!(
                db.query_row("SELECT signed_set FROM hosted_witness_trust", [], |r| {
                    r.get::<_, Option<Vec<u8>>>(0)
                })
                .expect("trust"),
                None
            );
        }
    }
}

fn seed_signer(f: &Value, role: &str) -> Ed25519Signer {
    Ed25519Signer::from_seed(
        &hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed")).expect("bytes"),
    )
    .expect("signer")
}
fn typed_sign<T: hybrid_codec::Canonical>(
    signer: &Ed25519Signer,
    domain: &str,
    body: &T,
) -> wire::AuthorizationSignature {
    wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(signer.public_key()),
        signature: signer
            .sign(&hybrid_codec::signing_digest(domain, body).expect("digest"))
            .expect("signature"),
    }
}
fn transfer_a_to_b(
    f: &Value,
    a: &Authority,
    b: &heddleco_capability_verifier::VerifiedOwnerState,
    b_key: &Ed25519Signer,
    sequence: u64,
    nonce: u8,
    previous: Vec<u8>,
) -> wire::ResourceTransferAuditRecord {
    fn counted(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend((bytes.len() as u32).to_be_bytes());
        out.extend(bytes);
    }
    let source = a
        .owner
        .signed_root()
        .root
        .as_ref()
        .expect("root")
        .account_uuid
        .clone();
    let destination = b
        .signed_root()
        .root
        .as_ref()
        .expect("root")
        .account_uuid
        .clone();
    let handoff = wire::ResourceTransferHandoff {
        format_version: 1,
        resource_uuid: a.ring.owner_genesis().spool_uuid().to_vec(),
        transfer_sequence: sequence,
        source_owner_uuid: source,
        source_owner_key_state_hash: a.owner.state_hash().to_vec(),
        destination_owner_uuid: destination,
        destination_owner_key_state_hash: b.state_hash().to_vec(),
        nonce: vec![nonce; 32],
    };
    let mut bytes = 1u32.to_be_bytes().to_vec();
    counted(&mut bytes, &handoff.resource_uuid);
    bytes.extend(sequence.to_be_bytes());
    for v in [
        &handoff.source_owner_uuid,
        &handoff.source_owner_key_state_hash,
        &handoff.destination_owner_uuid,
        &handoff.destination_owner_key_state_hash,
        &handoff.nonce,
    ] {
        counted(&mut bytes, v);
    }
    let signature = crypto::owner_root::sign_canonical(
        &seed_signer(f, "owner"),
        b"heddle-resource-transfer-handoff-v1",
        &bytes,
    )
    .expect("source signature");
    let mut accepted = Vec::new();
    counted(&mut accepted, &bytes);
    counted(&mut accepted, &signature.signer_key_id);
    counted(&mut accepted, &signature.signature);
    let destination_signature = crypto::owner_root::sign_canonical(
        b_key,
        b"heddle-resource-transfer-acceptance-v1",
        &accepted,
    )
    .expect("destination signature");
    let mut audit = wire::ResourceTransferAuditRecord {
        transfer: Some(wire::ResourceOwnershipTransfer {
            acceptance: Some(wire::ResourceTransferAcceptance {
                signed_handoff: Some(wire::SignedResourceTransferHandoff {
                    handoff: Some(handoff),
                    source_signature: Some(signature),
                }),
                destination_signature: Some(destination_signature),
            }),
        }),
        previous_audit_record_hash: previous,
        audit_record_hash: vec![],
        committed_at_unix_seconds: 1190,
    };
    audit.audit_record_hash = heddleco_capability_verifier::resource_transfer_audit_hash(&audit)
        .expect("audit hash")
        .to_vec();
    audit
}
struct TransferredAuthority {
    initial: Authority,
    owner: heddleco_capability_verifier::VerifiedOwnerState,
    ring: heddleco_capability_verifier::VerifiedCloneKeyring,
    wrong_ring: heddleco_capability_verifier::VerifiedCloneKeyring,
    mode: AtomicU64,
}
impl TransferredAuthority {
    fn selection(
        &self,
        sequence: u64,
    ) -> heddleco_capability_verifier::import_delegation::Selection<'_> {
        if sequence == 0 && self.mode.load(Ordering::SeqCst) != 3 {
            return self.initial.selection();
        }
        let mut selected = self.initial.selection();
        selected.owner = if self.mode.load(Ordering::SeqCst) == 2 {
            &self.initial.owner
        } else {
            &self.owner
        };
        selected.keyring = if self.mode.load(Ordering::SeqCst) == 1 {
            &self.wrong_ring
        } else {
            &self.ring
        };
        selected
    }
}
impl super::delegated_import::AcceptedAuthority for TransferredAuthority {
    fn authorize_import(&self, _: &wire::ImportPublicProofBundleV1, now: i64) -> Result<()> {
        self.ring.verify_current_owner(
            &self.owner,
            now / 1000,
            heddleco_capability_verifier::VerificationLimits::new(3600)?,
        )?;
        Ok(())
    }
    fn for_witness(
        &self,
        s: &host::HostedWitnessStatementV1,
    ) -> Result<heddleco_capability_verifier::import_delegation::Selection<'_>> {
        Ok(self.selection(s.ownership_transfer_sequence))
    }
    fn for_policy(
        &self,
        p: &wire::SignedPolicyBody,
    ) -> Result<heddleco_capability_verifier::import_delegation::Selection<'_>> {
        Ok(self.selection(p.ownership_transfer_sequence))
    }
    fn import_revoked(
        &self,
        _: &host::HostedWitnessStatementV1,
        _: heddleco_capability_verifier::import_delegation::Revocation<'_>,
    ) -> bool {
        false
    }
    fn native_revoked(
        &self,
        _: &host::HostedWitnessStatementV1,
        _: heddleco_capability_verifier::thread_control_authority::Revocation<'_>,
    ) -> bool {
        false
    }
}
fn transferred_bundle(
    f: &Value,
    b_device: &Ed25519Signer,
) -> (
    wire::ImportPublicProofBundleV1,
    TransferredAuthority,
    wire::ResourceTransferAuditRecord,
) {
    use api::import_authority as contract;
    let initial = Authority::new(f);
    let b_key = Ed25519Signer::from_seed(&[71; 32]).expect("B key");
    let b_recovery = Ed25519Signer::from_seed(&[73; 32]).expect("B recovery");
    let root =
        crypto::owner_root::sign_custodial_owner_root(&b_key, &b_recovery, [71; 16], [71; 32])
            .expect("B root");
    let owner = heddleco_capability_verifier::verify_owner_root(&root).expect("B authority");
    let transfer = transfer_a_to_b(f, &initial, &owner, &b_key, 1, 74, vec![]);
    let wrong = transfer_a_to_b(f, &initial, &owner, &b_key, 1, 75, vec![]);
    let fork = transfer_a_to_b(
        f,
        &initial,
        &owner,
        &b_key,
        2,
        76,
        transfer.audit_record_hash.clone(),
    );
    let h = wire::OwnerHistory {
        root: Some(root),
        accepted_transitions: vec![],
        state_hash: owner.state_hash().to_vec(),
    };
    let mut bundle: wire::ImportPublicProofBundleV1 = record(f, "complete_renewed_export");
    bundle.owner_histories.push(h);
    bundle.ownership_transfers = vec![transfer];
    let mut w = initial.ring.wire().clone();
    w.ownership_transfers = bundle.ownership_transfers.clone();
    w.transfer_owner_histories = bundle.owner_histories.clone();
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let ring = heddleco_capability_verifier::verify_clone_keyring(w.clone(), 1350, limits, &[])
        .expect("genuine A to B chain");
    w.ownership_transfers = vec![wrong];
    let wrong_ring = heddleco_capability_verifier::verify_clone_keyring(w, 1350, limits, &[])
        .expect("different genuine A to B prefix");
    let chain = wire::ImportOwnerChainV1 {
        spool_genesis_digest: initial.digest.to_vec(),
        owner_state_hashes: {
            let mut hashes = vec![
                initial.owner.state_hash().to_vec(),
                owner.state_hash().to_vec(),
            ];
            hashes.sort();
            hashes
        },
        transfer_audit_hashes: bundle
            .ownership_transfers
            .iter()
            .map(|t| t.audit_record_hash.clone())
            .collect(),
    };
    let digest = contract::owner_chain_digest(&chain).expect("chain digest");
    let mut identity: wire::ImportIdentityV1 = record(f, "identity");
    identity.owner_id = owner.owner_id().to_vec();
    identity.owner_account_uuid = owner
        .signed_root()
        .root
        .as_ref()
        .expect("root")
        .account_uuid
        .clone();
    identity.owner_state_hash = owner.state_hash().to_vec();
    identity.ownership_transfer_sequence = 1;
    let mut parent: wire::SignedImportMemberPermissionV1 = record(f, "renewed_permission");
    let p = parent.body.as_mut().expect("body");
    p.identity = Some(identity.clone());
    p.owner_chain_digest = digest.clone();
    p.subject_public_key = b_device.public_key().to_vec();
    parent.owner_signature = Some(typed_sign(&b_key, contract::PERMISSION_DOMAIN, p));
    let mut d: wire::SignedImportJobDelegationV1 = record(f, "renewed_delegation");
    let old_digest = contract::signed_delegation_digest(&d).expect("old digest");
    let b = d.body.as_mut().expect("body");
    b.identity = Some(identity);
    b.owner_chain_digest = digest;
    b.delegating_public_key = b_device.public_key().to_vec();
    b.parent_permission_digest = contract::signed_permission_digest(&parent).expect("parent");
    d.delegating_signature = Some(typed_sign(b_device, contract::DELEGATION_DOMAIN, b));
    let next_digest = contract::signed_delegation_digest(&d).expect("new digest");
    bundle.delegations[1] = d.clone();
    bundle
        .member_permissions
        .retain(|p| p != &record(f, "renewed_permission"));
    bundle.member_permissions.push(parent);
    bundle
        .member_permissions
        .sort_by_key(|p| contract::signed_permission_digest(p).expect("digest"));
    let renewal = bundle.renewals[0].body.as_mut().expect("renewal");
    renewal.replacement = Some(d);
    bundle.renewals[0].delegating_signature =
        Some(typed_sign(b_device, contract::RENEWAL_DOMAIN, renewal));
    let op = bundle
        .operations
        .iter_mut()
        .find(|o| {
            o.body
                .as_ref()
                .is_some_and(|b| b.delegation_digest == old_digest)
        })
        .expect("post-transfer result");
    let old_op = contract::signed_operation_digest(op).expect("old result");
    op.body.as_mut().expect("body").delegation_digest = next_digest.clone();
    op.job_signature = Some(typed_sign(
        &seed_signer(f, "renew_job"),
        contract::OPERATION_DOMAIN,
        op.body.as_ref().expect("body"),
    ));
    let operation = op.clone();
    let op_digest = contract::signed_operation_digest(op).expect("result digest");
    for manifest in &mut bundle.manifests {
        for slot in &mut manifest.slots {
            if slot.signed_operation_digest == old_op {
                slot.signed_operation_digest = op_digest.clone();
            }
        }
    }
    let terminal = bundle.terminal_manifest.as_mut().expect("terminal");
    for slot in &mut terminal.slots {
        if slot.signed_operation_digest == old_op {
            slot.signed_operation_digest = op_digest.clone();
        }
    }
    bundle
        .manifests
        .sort_by_key(|m| contract::manifest_digest(m).expect("digest"));
    let statement = bundle
        .statements
        .iter_mut()
        .find(|s| {
            s.body
                .as_ref()
                .is_some_and(|b| b.purpose == 3 && b.authority_digest == old_digest)
        })
        .expect("renewed publication");
    let s = statement.body.as_mut().expect("body");
    s.owner_id = owner.owner_id().to_vec();
    s.owner_state_hash = owner.state_hash().to_vec();
    s.ownership_transfer_sequence = 1;
    s.authority_digest = next_digest;
    s.original_signatures_digest = hybrid_codec::hash(&[&operation
        .job_signature
        .as_ref()
        .expect("signature")
        .signature]);
    s.canonical_payload = hybrid_codec::canonical(
        &contract::publication_payload(&operation, terminal).expect("publication"),
    )
    .expect("canonical publication");
    statement.signature = seed_signer(f, "witness")
        .sign(&witness_trust::statement_signing_digest(s).expect("digest"))
        .expect("authentic post-transfer observation");
    let mut set: host::SignedHostedWitnessSetV1 = record(f, "current_set");
    set.body.as_mut().expect("body").generation += 1;
    set.body.as_mut().expect("body").issued_at_unix_millis = 1350000;
    set.body.as_mut().expect("body").valid_until_unix_millis = 1650000;
    set.body
        .as_mut()
        .expect("body")
        .entries
        .iter_mut()
        .find(|e| e.state == 1)
        .expect("current")
        .active_until_unix_millis = 2000000;
    resign(f, &mut set);
    bundle.witness_set = Some(set);
    bundle.history_proofs.clear();
    contract::validate_public_bundle(&bundle).expect("complete transfer/renewal public bundle");
    (
        bundle,
        TransferredAuthority {
            initial,
            owner,
            ring,
            wrong_ring,
            mode: AtomicU64::new(0),
        },
        fork,
    )
}
#[test]
fn review_transfer_preserves_original_genesis_and_exact_historical_prefixes() {
    let f = fixture();
    let device = Ed25519Signer::from_seed(&[72; 32]).expect("B device");
    let (bundle, authority, fork) = transferred_bundle(&f, &device);
    let records = [record(&f, "converted_main"), record(&f, "converted_dev")];
    for existing in [true, false] {
        let dir = tempfile::tempdir().expect("dir");
        let repo = crate::Repository::init_default(dir.path()).expect("repo");
        let selected = root(&f);
        select_root(repo.heddle_dir(), &selected).expect("root");
        select_spool(
            repo.heddle_dir(),
            authority.initial.ring.owner_genesis().spool_uuid(),
            authority.initial.digest,
            authority.initial.initial,
        )
        .expect("Spool");
        let trust = HostedTrust::open(
            repo.heddle_dir(),
            &selected.authority,
            TestClock::new(1350000),
        )
        .expect("trust");
        let install = |b: &wire::ImportPublicProofBundleV1| {
            ThreadReplica::install_hybrid_import(
                repo.heddle_dir(),
                &trust,
                &b.encode_to_vec(),
                &records,
                &authority,
                repo.store(),
            )
        };
        if existing {
            install(&bundle).expect("existing mixed-history control");
        }
        let db = crate::local_metadata::open(repo.heddle_dir()).expect("db");
        let before: i64 = db
            .query_row("SELECT count(*) FROM hosted_import_admissions", [], |r| {
                r.get(0)
            })
            .expect("count");
        for mode in [1, 2, 3] {
            authority.mode.store(mode, Ordering::SeqCst);
            let error = install(&bundle)
                .err()
                .expect("wrong prefix/owner must reject");
            if mode != 2 {
                assert!(matches!(error, Error::Hybrid(hybrid_codec::Reject::Root)));
            } else {
                assert!(matches!(
                    error,
                    Error::ImportAuthority(heddleco_capability_verifier::Error::BrokenChain(_))
                ));
            }
            assert_eq!(
                db.query_row("SELECT count(*) FROM hosted_import_admissions", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .expect("count"),
                before
            );
        }
        authority.mode.store(0, Ordering::SeqCst);
        let mut forked = bundle.clone();
        forked.ownership_transfers.push(fork.clone());
        assert!(
            matches!(install(&forked),Err(Error::ImportAuthority(heddleco_capability_verifier::Error::BrokenChain(reason))) if reason.contains("forks from a non-current owner"))
        );
        let replicas = install(&bundle).expect("genuine mixed A to B history");
        assert_eq!(replicas.len(), 2);
        for r in replicas {
            let original = r.hybrid_import_bundle().expect("proofs").expect("bundle");
            assert_eq!(original.original_geneses, bundle.original_geneses);
            assert_eq!(original.genesis_authorities, bundle.genesis_authorities);
            assert_eq!(original.ownership_transfers, bundle.ownership_transfers);
        }
    }
}

#[test]
fn review_fresh_bundle_job_cannot_become_post_transfer_delegator() {
    let f = fixture();
    let records = [record(&f, "converted_main"), record(&f, "converted_dev")];
    for role in ["device", "job"] {
        let device = if role == "device" {
            Ed25519Signer::from_seed(&[72; 32]).expect("B device")
        } else {
            seed_signer(&f, "job")
        };
        let (bundle, authority, _) = transferred_bundle(&f, &device);
        let dir = tempfile::tempdir().expect("dir");
        let repo = crate::Repository::init_default(dir.path()).expect("repo");
        let selected = root(&f);
        select_root(repo.heddle_dir(), &selected).expect("root");
        select_spool(
            repo.heddle_dir(),
            authority.initial.ring.owner_genesis().spool_uuid(),
            authority.initial.digest,
            authority.initial.initial,
        )
        .expect("Spool");
        let trust = HostedTrust::open(
            repo.heddle_dir(),
            &selected.authority,
            TestClock::new(1350000),
        )
        .expect("trust");
        let result = ThreadReplica::install_hybrid_import(
            repo.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &authority,
            repo.store(),
        );
        if role == "device" {
            assert_eq!(
                result.expect("genuine post-transfer device control").len(),
                2
            );
        } else {
            let error = result
                .err()
                .expect("genuine B-signed permission cannot promote the earlier job to device");
            assert!(
                matches!(
                    error,
                    Error::ImportAuthority(heddleco_capability_verifier::Error::Hybrid(
                        hybrid_codec::Reject::KeyRole
                    ))
                ),
                "wrong role gate: {error:?}"
            );
            let db = crate::local_metadata::open(repo.heddle_dir()).expect("db");
            for table in [
                "hosted_import_job_keys",
                "hosted_import_admissions",
                "hosted_import_proofs",
                "hosted_import_slots",
            ] {
                assert_eq!(
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .expect("count"),
                    0
                );
            }
        }
    }
}
