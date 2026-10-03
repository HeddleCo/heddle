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
        Err(Error::Hybrid(hybrid_codec::Reject::StaleContext))
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
        subject: NativeSubject::Authority(&payload),
    };
    assert_eq!(
        replica
            .receive_witnessed(&trust, &input, &a, repo.store(), |_| Ok(()))
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
        replica.receive_witnessed(&trust, &revoked, &a, repo.store(), |_| Ok(())),
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
