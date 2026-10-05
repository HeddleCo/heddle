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
    fn authorize_import(
        &self,
        _: &wire::ImportPublicProofBundleV1,
        now_millis: i64,
        _: &TrustTransaction<'_>,
    ) -> Result<()> {
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
        |_| Ok(()),
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
        |_| Ok(()),
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
    assert_eq!(
        trust
            .snapshot()
            .expect("job snapshot")
            .known_job_associations
            .len(),
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
            repo.store(),
            |_| Ok(())
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
        |_| Ok(()),
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
            repo.store(),
            |_| Ok(())
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
            repo.store(),
            |_| Ok(())
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
        |_| Ok(()),
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
            repo.store(),
            |_| Ok(())
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
            repo.store(),
            |_| Ok(())
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
        |_| Ok(()),
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
    fn authorize_import(
        &self,
        _: &wire::ImportPublicProofBundleV1,
        now: i64,
        _: &TrustTransaction<'_>,
    ) -> Result<()> {
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
            |_| Ok(()),
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
    fn authorize_import(
        &self,
        _: &wire::ImportPublicProofBundleV1,
        now: i64,
        _: &TrustTransaction<'_>,
    ) -> Result<()> {
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
                |_| Ok(()),
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
            |_| Ok(()),
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

struct HybridReceiver {
    _directory: tempfile::TempDir,
    repo: crate::Repository,
    trust: HostedTrust<TestClock>,
    clock: TestClock,
    authority: Authority,
    bundle: wire::ImportPublicProofBundleV1,
    fixture: Value,
}
impl HybridReceiver {
    fn new() -> Self {
        let fixture = fixture();
        let directory = tempfile::tempdir().expect("receiver");
        let repo = crate::Repository::init_default(directory.path()).expect("repository");
        let selected = root(&fixture);
        select_root(repo.heddle_dir(), &selected).expect("independent root");
        let authority = Authority::new(&fixture);
        select_spool(
            repo.heddle_dir(),
            authority.ring.owner_genesis().spool_uuid(),
            authority.digest,
            authority.initial,
        )
        .expect("independent Spool");
        let clock = TestClock::new(1_350_000);
        let trust = HostedTrust::open(repo.heddle_dir(), &selected.authority, clock.clone())
            .expect("trust");
        let mut bundle: wire::ImportPublicProofBundleV1 =
            record(&fixture, "complete_renewed_export");
        bundle.history_proofs = [
            "genesis_proof",
            "genesis_dev_proof",
            "publication_proof",
            "renewed_publication_proof",
        ]
        .iter()
        .map(|name| record(&fixture, name))
        .collect();
        Self {
            _directory: directory,
            repo,
            trust,
            clock,
            authority,
            bundle,
            fixture,
        }
    }
    fn counts(&self) -> Vec<i64> {
        let db = crate::local_metadata::open(self.repo.heddle_dir()).expect("database");
        [
            "threads",
            "operations",
            "hosted_import_proofs",
            "hosted_import_admissions",
            "hosted_import_slots",
            "hosted_import_job_keys",
        ]
        .map(|table| {
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count")
        })
        .to_vec()
    }
}

#[test]
fn hybrid_selected_genesis_does_not_install_unrelated_carried_branch() {
    let receiver = HybridReceiver::new();
    let selected = receiver.bundle.original_geneses[0].clone();
    let (_, genesis) = crypto::import_authority::verify_native_genesis(&selected).expect("genesis");
    let replicas = ThreadReplica::install_hybrid_import(
        receiver.repo.heddle_dir(),
        &receiver.trust,
        &receiver.bundle.encode_to_vec(),
        &[selected],
        &receiver.authority,
        &objects::store::InMemoryStore::new(),
        |_| Ok(()),
    )
    .expect("selected genesis without converted operations");
    assert_eq!(
        replicas.len(),
        1,
        "public evidence must not install another branch"
    );
    assert_eq!(replicas[0].thread, genesis.id().expect("id"));
    let other = receiver
        .bundle
        .original_geneses
        .iter()
        .find(|record| {
            crypto::import_authority::verify_native_genesis(record)
                .expect("genesis")
                .1
                .id()
                .expect("id")
                != replicas[0].thread
        })
        .expect("other branch");
    assert!(
        ThreadReplica::open(
            receiver.repo.heddle_dir(),
            crypto::import_authority::verify_native_genesis(other)
                .expect("genesis")
                .1
                .id()
                .expect("id")
        )
        .is_err()
    );
    assert_eq!(
        replicas[0].hybrid_import_bundle().expect("bundle"),
        Some(receiver.bundle)
    );
}

#[test]
fn hybrid_selected_capture_installs_its_genesis_and_checks_unselected_history() {
    let receiver = HybridReceiver::new();
    let selected: wire::SignedRecord = record(&receiver.fixture, "converted_main");
    let (_, operation) =
        crypto::import_authority::verify_native_operation(&selected).expect("capture");
    // Tamper only with an unselected branch's public publication signature.
    let mut bad = receiver.bundle.clone();
    bad.operations
        .iter_mut()
        .find(|op| op.body.as_ref().expect("body").target_thread_id != operation.thread.as_bytes())
        .expect("other public branch")
        .job_signature
        .as_mut()
        .expect("job signature")
        .signature[0] ^= 1;
    let called = AtomicBool::new(false);
    assert!(
        ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &bad.encode_to_vec(),
            std::slice::from_ref(&selected),
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |_| {
                called.store(true, Ordering::SeqCst);
                Ok(())
            }
        )
        .is_err()
    );
    assert!(!called.load(Ordering::SeqCst));
    let replicas = ThreadReplica::install_hybrid_import(
        receiver.repo.heddle_dir(),
        &receiver.trust,
        &receiver.bundle.encode_to_vec(),
        &[selected],
        &receiver.authority,
        &objects::store::InMemoryStore::new(),
        |_| Ok(()),
    )
    .expect("selected capture with genesis dependency");
    assert_eq!(replicas.len(), 1);
    assert_eq!(replicas[0].thread, operation.thread);
    assert!(
        replicas[0]
            .hosted_admission(operation.id().expect("id"))
            .expect("admission")
            .is_some()
    );
    assert!(
        replicas[0]
            .hosted_admission(operation.thread)
            .expect("genesis admission")
            .is_some()
    );
}

#[test]
fn hybrid_uncovered_original_rejects_before_callback() {
    let receiver = HybridReceiver::new();
    let payload: wire::ImportAuthorityWitnessV1 =
        record(&receiver.fixture, "authority_admission_payload");
    let uncovered = payload.original.expect("genuinely signed native original");
    crypto::import_authority::verify_native_operation(&uncovered).expect("signature control");
    let mut selected = vec![
        record(&receiver.fixture, "converted_main"),
        record(&receiver.fixture, "converted_dev"),
    ];
    selected.extend(payload.dependencies);
    selected.push(uncovered);
    let before = receiver.counts();
    let called = AtomicBool::new(false);
    assert!(matches!(
        ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &receiver.bundle.encode_to_vec(),
            &selected,
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |_| {
                called.store(true, Ordering::SeqCst);
                Ok(())
            }
        ),
        Err(Error::Hybrid(hybrid_codec::Reject::ImportPermission))
    ));
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(receiver.counts(), before);
}

#[test]
fn hybrid_late_rejection_restores_pack_pins_metadata_and_sql() {
    for failure in ["callback", "expiry", "rollback", "disclosure"] {
        let receiver = HybridReceiver::new();
        let pack = receiver.repo.heddle_dir().join("selected.pack");
        let spool = receiver.repo.heddle_dir().join("native-spool-id");
        let owner = receiver.repo.heddle_dir().join("selected-owner-metadata");
        std::fs::write(&spool, b"previous spool").expect("existing pin");
        std::fs::write(&owner, b"previous owner").expect("existing metadata");
        let mut staged = tempfile::NamedTempFile::new().expect("isolated pack");
        std::io::Write::write_all(&mut staged, b"verified staged pack").expect("staged pack");
        let before = receiver.counts();
        let selected = [record(&receiver.fixture, "converted_main")];
        let called = AtomicBool::new(false);
        let result = ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &receiver.bundle.encode_to_vec(),
            &selected,
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |artifacts| {
                called.store(true, Ordering::SeqCst);
                // The SQL writer transaction remains uncommitted and held.
                let second = rusqlite::Connection::open(
                    receiver
                        .repo
                        .heddle_dir()
                        .join(crate::local_metadata::DATABASE_NAME),
                )?;
                second.busy_timeout(std::time::Duration::ZERO)?;
                assert!(second.execute_batch("BEGIN IMMEDIATE").is_err());
                artifacts.install_file(
                    staged.path(),
                    pack.strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                )?;
                artifacts.write_file(
                    spool
                        .strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                    b"new spool",
                )?;
                artifacts.write_file(
                    owner
                        .strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                    b"new owner",
                )?;
                artifacts.write_file(
                    owner
                        .strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                    b"second write",
                )?;
                match failure {
                    "callback" => return Err(Error::Hybrid(hybrid_codec::Reject::Revoked)),
                    "expiry" => receiver.clock.set(
                        receiver
                            .bundle
                            .witness_set
                            .as_ref()
                            .expect("set")
                            .body
                            .as_ref()
                            .expect("body")
                            .valid_until_unix_millis,
                    ),
                    "rollback" => receiver.clock.set(1_349_999),
                    "disclosure" => receiver.authority.disclosure.store(false, Ordering::SeqCst),
                    _ => unreachable!("test case"),
                }
                Ok(())
            },
        );
        assert!(result.is_err(), "late {failure} must reject");
        assert!(
            called.load(Ordering::SeqCst),
            "test must reach the callback"
        );
        assert!(
            !pack.exists(),
            "late {failure} must remove the installed pack"
        );
        assert_eq!(
            std::fs::read(&spool).expect("restored pin"),
            b"previous spool"
        );
        assert_eq!(
            std::fs::read(&owner).expect("restored metadata"),
            b"previous owner"
        );
        assert_eq!(receiver.counts(), before);
        receiver.clock.set(1_350_000);
        receiver.authority.disclosure.store(true, Ordering::SeqCst);
        ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &receiver.bundle.encode_to_vec(),
            &selected,
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |artifacts| {
                artifacts.install_file(
                    staged.path(),
                    pack.strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                )?;
                artifacts.write_file(
                    spool
                        .strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                    b"new spool",
                )?;
                artifacts.write_file(
                    owner
                        .strip_prefix(receiver.repo.heddle_dir())
                        .expect("relative artifact"),
                    b"new owner",
                )
            },
        )
        .expect("successful atomic installation control");
        assert_eq!(
            std::fs::read(pack).expect("committed pack"),
            b"verified staged pack"
        );
        assert_eq!(std::fs::read(spool).expect("committed pin"), b"new spool");
        assert_eq!(
            std::fs::read(owner).expect("committed metadata"),
            b"new owner"
        );
    }
}

#[test]
fn hybrid_snapshot_does_not_mutate_floor_and_restores_retained_root_history() {
    let receiver = HybridReceiver::new();
    receiver
        .trust
        .mutate(receiver.bundle.witness_set.as_ref().expect("set"), |_| {
            Ok(())
        })
        .expect("persist accepted set");
    let original = receiver.trust.snapshot().expect("snapshot");
    receiver.clock.set(1_350_100);
    assert_eq!(
        receiver
            .trust
            .snapshot()
            .expect("later read")
            .clock_floor_millis,
        original.clock_floor_millis,
        "an unexpired read must not persist receiver time"
    );
    let selected = root(&receiver.fixture);
    let replacement = RootSelection {
        root_id: "replacement-root".into(),
        public_key: key(&receiver.fixture, "owner"),
        ..selected.clone()
    };
    replace_root(receiver.repo.heddle_dir(), &selected, &replacement)
        .expect("explicit independent replacement");
    let expired_at = receiver
        .bundle
        .witness_set
        .as_ref()
        .expect("set")
        .body
        .as_ref()
        .expect("body")
        .valid_until_unix_millis;
    receiver.clock.set(expired_at + 1);
    let before = crate::local_metadata::open(receiver.repo.heddle_dir()).expect("db").query_row("SELECT signed_set,clock_floor,history_root_id,history_root_key FROM hosted_witness_trust", [], |r| Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Vec<u8>>(3)?))).expect("durable row");
    let snapshot = receiver
        .trust
        .snapshot()
        .expect("expired set remains a history floor under its original root");
    assert_eq!(snapshot.root, replacement);
    assert_eq!(snapshot.root_epoch, 2);
    assert_eq!(snapshot.clock_floor_millis, original.clock_floor_millis);
    assert_eq!(
        snapshot.previous.as_ref().expect("previous").digest(),
        original.previous.as_ref().expect("previous").digest()
    );
    assert_eq!(
        snapshot.previous.as_ref().expect("previous").body(),
        receiver
            .bundle
            .witness_set
            .as_ref()
            .expect("original set")
            .body
            .as_ref()
            .expect("body")
    );
    assert!(snapshot.known_job_associations.is_empty());
    let after = crate::local_metadata::open(receiver.repo.heddle_dir()).expect("db").query_row("SELECT signed_set,clock_floor,history_root_id,history_root_key FROM hosted_witness_trust", [], |r| Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Vec<u8>>(3)?))).expect("durable row");
    assert_eq!(before, after, "reading must never advance durable trust");
}

#[test]
fn hybrid_snapshot_rejects_clock_below_durable_floor() {
    let receiver = HybridReceiver::new();
    // Simulate a receiver restarted with a persisted wall floor and no
    // process anchor. No signed set can bypass this check, even absent history.
    crate::local_metadata::open(receiver.repo.heddle_dir())
        .expect("db")
        .execute("UPDATE hosted_witness_trust SET clock_floor=1350001", [])
        .expect("persisted restart floor");
    assert!(matches!(receiver.trust.snapshot(), Err(Error::HostedClock)));
    receiver.clock.set(1_350_001);
    assert_eq!(
        receiver
            .trust
            .snapshot()
            .expect("trustworthy clock control")
            .clock_floor_millis,
        1_350_001
    );
}

#[test]
fn hybrid_snapshot_rejects_frozen_clock_across_handles() {
    let receiver = HybridReceiver::new();
    receiver.trust.snapshot().expect("read anchor");
    receiver.clock.elapsed.store(100, Ordering::SeqCst);
    let another = HostedTrust::open(
        receiver.repo.heddle_dir(),
        &root(&receiver.fixture).authority,
        receiver.clock.clone(),
    )
    .expect("another handle");
    assert!(matches!(another.snapshot(), Err(Error::HostedClock)));
    receiver.clock.set(1_350_100);
    another.snapshot().expect("restored trustworthy time");
}

#[test]
fn hybrid_selected_native_control_installs_with_its_original_admission() {
    let mut receiver = HybridReceiver::new();
    let payload: wire::ImportAuthorityWitnessV1 =
        record(&receiver.fixture, "authority_admission_payload");
    let original = payload.original.clone().expect("original");
    let (_, operation) =
        crypto::import_authority::verify_native_operation(&original).expect("native control");
    receiver.bundle.authority_witnesses.push(payload);
    receiver
        .bundle
        .statements
        .push(record(&receiver.fixture, "authority_admission"));
    receiver
        .bundle
        .history_proofs
        .push(record(&receiver.fixture, "authority_proof"));
    // The public bundle already carries the independently verified policy.
    let selected = [record(&receiver.fixture, "converted_main"), original];
    let replicas = ThreadReplica::install_hybrid_import(
        receiver.repo.heddle_dir(),
        &receiver.trust,
        &receiver.bundle.encode_to_vec(),
        &selected,
        &receiver.authority,
        &objects::store::InMemoryStore::new(),
        |_| Ok(()),
    )
    .expect("native original and selected causal closure");
    assert_eq!(replicas.len(), 1);
    let replica = &replicas[0];
    assert_eq!(replica.thread, operation.thread);
    assert_eq!(
        replica
            .hosted_admission(operation.id().expect("id"))
            .expect("admission")
            .expect("own admission")
            .statement,
        record(&receiver.fixture, "authority_admission")
    );
}

#[test]
fn hybrid_selected_boundary_original_installs_only_verified_receipt_dependencies() {
    let mut receiver = HybridReceiver::new();
    receiver.bundle.genesis_witnesses = [
        record(&receiver.fixture, "boundary_genesis_payload"),
        record(&receiver.fixture, "boundary_dev_genesis_payload"),
    ]
    .to_vec();
    receiver
        .bundle
        .statements
        .retain(|statement| statement.body.as_ref().expect("body").purpose != 1);
    receiver.bundle.statements.extend([
        record(&receiver.fixture, "boundary_genesis_statement"),
        record(&receiver.fixture, "boundary_dev_genesis_statement"),
    ]);
    receiver.bundle.history_proofs.extend([
        record(&receiver.fixture, "boundary_genesis_proof"),
        record(&receiver.fixture, "boundary_dev_genesis_proof"),
        record(&receiver.fixture, "boundary_authority_proof"),
    ]);
    let payload: wire::ImportAuthorityWitnessV1 =
        record(&receiver.fixture, "boundary_authority_payload");
    let original = payload.original.clone().expect("original");
    let (_, operation) =
        crypto::import_authority::verify_native_operation(&original).expect("native original");
    receiver.bundle.authority_witnesses.push(payload);
    receiver
        .bundle
        .statements
        .push(record(&receiver.fixture, "boundary_authority_statement"));
    let replicas = ThreadReplica::install_hybrid_import(
        receiver.repo.heddle_dir(),
        &receiver.trust,
        &receiver.bundle.encode_to_vec(),
        &[original],
        &receiver.authority,
        &objects::store::InMemoryStore::new(),
        |_| Ok(()),
    )
    .expect("individually authenticated native boundary receipts");
    assert_eq!(replicas.len(), 1);
    assert_eq!(replicas[0].thread, operation.thread);
    assert!(
        replicas[0]
            .hosted_admission(operation.id().expect("id"))
            .expect("admission")
            .is_some()
    );
}

#[test]
fn hybrid_callback_and_post_callback_panics_restore_before_unwinding() {
    for hook in [false, true] {
        let receiver = HybridReceiver::new();
        std::fs::write(receiver.repo.heddle_dir().join("panic-pin"), b"old").expect("old");
        let before = receiver.counts();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            receiver.trust.mutate_with_artifacts(
                receiver.bundle.witness_set.as_ref().expect("set"),
                |context| {
                    context.sql().execute(
                        "INSERT INTO hosted_import_job_keys VALUES(?1,?2)",
                        rusqlite::params![[98u8; 32], [98u8; 16]],
                    )?;
                    Ok(())
                },
                |_, _| Ok(()),
                |_, writer| {
                    writer.write_file(std::path::Path::new("panic-pin"), b"new")?;
                    writer.write_file(std::path::Path::new("panic-pack"), b"pack")?;
                    if !hook {
                        panic!("callback panic");
                    }
                    Ok(())
                },
                |_, _| {
                    panic!("post-callback disclosure hook panic");
                },
            )
        }));
        assert!(result.is_err(), "must resume panic");
        assert_eq!(
            std::fs::read(receiver.repo.heddle_dir().join("panic-pin")).expect("restored"),
            b"old"
        );
        assert!(!receiver.repo.heddle_dir().join("panic-pack").exists());
        assert_eq!(receiver.counts(), before);
        receiver
            .trust
            .snapshot()
            .expect("clock anchor not poisoned");
        receiver
            .trust
            .mutate(receiver.bundle.witness_set.as_ref().expect("set"), |_| {
                Ok(())
            })
            .expect("usable after handled unwind");
    }
}

#[test]
fn hybrid_rollback_io_preserves_original_rejection_and_blocks_trusted_use() {
    use super::install_artifacts::tests::{clear_failure, fail_rollback};
    for failed in 0..4 {
        let receiver = HybridReceiver::new();
        for index in [0, 2] {
            std::fs::write(
                receiver.repo.heddle_dir().join(format!("pin{index}")),
                b"old",
            )
            .expect("old");
        }
        let before = receiver.counts();
        fail_rollback(failed, if failed % 2 == 0 { "rename" } else { "unlink" });
        let result = ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &receiver.bundle.encode_to_vec(),
            &[record(&receiver.fixture, "converted_main")],
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |writer| {
                for index in 0..4 {
                    writer.write_file(std::path::Path::new(&format!("pin{index}")), b"new")?;
                }
                Err(Error::Hybrid(hybrid_codec::Reject::Revoked))
            },
        );
        assert!(
            matches!(result, Err(Error::Hybrid(hybrid_codec::Reject::Revoked))),
            "original rejection must survive cleanup failure"
        );
        assert!(
            receiver.trust.snapshot().is_err(),
            "trusted snapshot must wait for undo"
        );
        assert!(
            HostedTrust::open(
                receiver.repo.heddle_dir(),
                &root(&receiver.fixture).authority,
                receiver.clock.clone()
            )
            .is_err()
        );
        assert!(
            crate::Repository::open(receiver.repo.root()).is_err(),
            "repository open must not expose uncommitted artifacts"
        );
        let mut called = false;
        assert!(
            ThreadReplica::install_hybrid_import(
                receiver.repo.heddle_dir(),
                &receiver.trust,
                &receiver.bundle.encode_to_vec(),
                &[record(&receiver.fixture, "converted_main")],
                &receiver.authority,
                &objects::store::InMemoryStore::new(),
                |_| {
                    called = true;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!called, "installer must wait for undo");
        clear_failure();
        receiver.trust.snapshot().expect("retry undo");
        assert_eq!(receiver.counts(), before);
        for index in 0..4 {
            let pin = receiver.repo.heddle_dir().join(format!("pin{index}"));
            if index % 2 == 0 {
                assert_eq!(std::fs::read(pin).expect("recovered"), b"old");
            } else {
                assert!(!pin.exists());
            }
        }
    }
}

#[test]
fn hybrid_failed_install_blocks_retained_repository_native_identity_until_retry() {
    use objects::store::ObjectStore as _;

    use super::install_artifacts::tests::{clear_failure, fail_rollback};
    let receiver = HybridReceiver::new();
    let original = receiver.repo.native_spool_id().expect("original spool");
    let replacement = uuid::Uuid::now_v7();
    let replica = receiver
        .repo
        .native_thread("main")
        .expect("original Thread");
    let base = replica.genesis().expect("genesis").base;
    let state = receiver
        .repo
        .store()
        .get_state(&base)
        .expect("base lookup")
        .expect("base State");
    let signed = replica.signed_genesis().expect("signed genesis");
    let owner = receiver
        .repo
        .native_original_owner_signer(&replica)
        .expect("owner");
    let owner_key = owner.public_key().try_into().expect("owner key");
    fail_rollback(0, "rename");
    let rejected = ThreadReplica::install_hybrid_import(
        receiver.repo.heddle_dir(),
        &receiver.trust,
        &receiver.bundle.encode_to_vec(),
        &[record(&receiver.fixture, "converted_main")],
        &receiver.authority,
        &objects::store::InMemoryStore::new(),
        |writer| {
            writer.write_file(
                std::path::Path::new("spool-id"),
                replacement.to_string().as_bytes(),
            )?;
            Err(Error::Hybrid(hybrid_codec::Reject::Revoked))
        },
    );
    assert!(matches!(
        rejected,
        Err(Error::Hybrid(hybrid_codec::Reject::Revoked))
    ));
    let intent = receiver.repo.heddle_dir().join("hosted-install.intent");
    assert!(intent.exists(), "failed undo retains its intent");
    let mut refused = Vec::new();
    for _ in 0..2 {
        refused.extend([
            ("spool read", receiver.repo.native_spool_id().is_err()),
            (
                "spool install",
                receiver.repo.install_native_spool_id(replacement).is_err(),
            ),
            (
                "create",
                receiver
                    .repo
                    .create_native_thread("unrecovered", base, None, "")
                    .is_err(),
            ),
            (
                "rename",
                receiver
                    .repo
                    .rename_native_thread("main", "unrecovered")
                    .is_err(),
            ),
            (
                "adopt",
                receiver
                    .repo
                    .adopt_native_thread("unrecovered", &signed)
                    .is_err(),
            ),
            (
                "capture",
                receiver.repo.record_native_capture("main", base).is_err(),
            ),
            (
                "owner key",
                receiver.repo.holds_native_owner_key(&owner_key).is_err(),
            ),
            (
                "original signer",
                receiver
                    .repo
                    .native_original_owner_signer(&replica)
                    .is_err(),
            ),
            (
                "Thread signer",
                receiver.repo.native_thread_signer(&replica).is_err(),
            ),
            (
                "Thread lookup",
                receiver.repo.native_thread("main").is_err(),
            ),
            ("Thread list", receiver.repo.list_native_threads().is_err()),
            (
                "client metadata signer",
                receiver.repo.sign_client_metadata(b"metadata").is_err(),
            ),
            ("auto signer", receiver.repo.signing_signer().is_none()),
            (
                "authored State",
                receiver.repo.put_authored_state(&state).is_err(),
            ),
            (
                "entry visibility",
                receiver
                    .repo
                    .get_entry_visibility_bytes(&state.change_id)
                    .is_err(),
            ),
            (
                "state visibility",
                receiver.repo.get_state_visibility_for_state(&base).is_err(),
            ),
            ("redactions", receiver.repo.list_all_redactions().is_err()),
            ("briefing", receiver.repo.pending_context_receipt().is_err()),
            ("partial fetch", receiver.repo.missing_blobs().is_err()),
            (
                "checkout manifest",
                receiver.repo.is_incomplete_checkout().is_err(),
            ),
            (
                "catalog binding",
                crate::device_catalog::load(&crate::identity::heddle_home_dir(), original).is_err(),
            ),
        ]);
    }
    let polluted = crate::device_catalog::store::Catalog::read(&crate::identity::heddle_home_dir())
        .expect("catalog")
        .expect("registered catalog")
        .spool(replacement)
        .expect("lookup")
        .is_some();
    assert!(intent.exists(), "persistent failure must retain undo");
    clear_failure();
    for (api, blocked) in refused {
        assert!(
            blocked,
            "{api} bypassed persistently failing recovery on a retained Repository"
        );
    }
    assert!(
        !polluted,
        "uncommitted spool must never enter the device catalog"
    );
    assert_eq!(
        receiver
            .repo
            .native_spool_id()
            .expect("same handle retries recovery"),
        original
    );
    assert!(!intent.exists(), "successful retry retires undo");
    let created = receiver
        .repo
        .create_native_thread("recovered", base, None, "")
        .expect("create after retry");
    assert_eq!(
        created.genesis().expect("genesis").spool,
        original.to_string()
    );
    assert!(
        receiver
            .repo
            .holds_native_owner_key(&owner_key)
            .expect("recovered owner key")
    );
    receiver
        .repo
        .sign_client_metadata(b"metadata")
        .expect("recovered metadata signer");
    receiver
        .repo
        .get_entry_visibility_bytes(&state.change_id)
        .expect("recovered entry visibility");
    receiver
        .repo
        .get_state_visibility_for_state(&base)
        .expect("recovered state visibility");
    receiver
        .repo
        .list_all_redactions()
        .expect("recovered redactions");
    receiver
        .repo
        .pending_context_receipt()
        .expect("recovered briefing");
    receiver
        .repo
        .missing_blobs()
        .expect("recovered partial fetch");
    receiver
        .repo
        .is_incomplete_checkout()
        .expect("recovered manifest");
    crate::device_catalog::load(&crate::identity::heddle_home_dir(), original)
        .expect("recovered catalog binding");
}

fn landing_receiver() -> (
    HybridReceiver,
    wire::HostedLandingWitnessV1,
    Vec<host::SignedHostedWitnessStatementV1>,
) {
    use hybrid_codec::Canonical;
    use objects::object::thread_replication::{
        SourceAuthor, ThreadOperationBody, metadata::ThreadControl,
    };
    let mut receiver = HybridReceiver::new();
    let landing: wire::HostedLandingWitnessV1 = record(&receiver.fixture, "landing_payload");
    // This test host's current executor covers the independent admissions at
    // the landing time; original source, review and landing bytes stay exact.
    let set = receiver.bundle.witness_set.as_mut().expect("set");
    let current = set
        .body
        .as_mut()
        .expect("body")
        .entries
        .iter_mut()
        .find(|entry| entry.state == 1)
        .expect("current witness");
    current.active_from_unix_millis = 0;
    let executor = current.executor_id.clone();
    resign(&receiver.fixture, set);
    let mut statements = Vec::new();
    for (index, original) in landing
        .source_operation
        .iter()
        .chain(&landing.review_evidence)
        .enumerate()
    {
        let (_, operation) =
            crypto::import_authority::verify_native_operation(original).expect("signed original");
        let envelope = match operation.body {
            ThreadOperationBody::Capture(capture) => match capture.author {
                SourceAuthor::Account { authority, .. } => authority,
                _ => panic!("account capture"),
            },
            ThreadOperationBody::Metadata(bytes) => {
                ThreadControl::decode(&bytes)
                    .expect("review")
                    .authority_envelope
            }
            _ => panic!("source/review original"),
        };
        let payload = wire::ImportAuthorityWitnessV1 {
            format_version: 1,
            kind: 1,
            original: Some(original.clone()),
            authority_envelope: envelope,
            ..Default::default()
        };
        let mut statement: host::SignedHostedWitnessStatementV1 =
            record(&receiver.fixture, "authority_admission");
        let body = statement.body.as_mut().expect("body");
        body.executor_id = executor.clone();
        body.canonical_payload = hybrid_codec::canonical(&payload).expect("payload");
        body.publisher_key_id = hybrid_codec::key_id(&original.signatures[0].public_key);
        body.authority_digest = hybrid_codec::hash(&[
            b"heddle-hosted-authority-envelope-v1",
            &(payload.authority_envelope.len() as u32).to_be_bytes(),
            &payload.authority_envelope,
        ]);
        let mut signatures = (original.signatures.len() as u32).to_be_bytes().to_vec();
        for signature in &original.signatures {
            signature.write(&mut signatures).expect("signature");
        }
        body.original_signatures_digest =
            hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &signatures]);
        body.admission_order = 97 + index as u64;
        body.host_transaction_id = vec![70 + index as u8; 16];
        statement.signature = seed_signer(&receiver.fixture, "next_witness")
            .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
            .expect("signed first admission");
        api::import_authority::verify_witness_payload(
            body,
            api::import_authority::WitnessPayload::Authority(&payload),
        )
        .expect("matched first admission");
        receiver.bundle.authority_witnesses.push(payload);
        receiver.bundle.statements.push(statement.clone());
        statements.push(statement);
    }
    receiver.bundle.landing_witnesses.push(landing.clone());
    receiver
        .bundle
        .statements
        .push(record(&receiver.fixture, "landing_statement"));
    receiver
        .bundle
        .history_proofs
        .push(record(&receiver.fixture, "landing_proof"));
    (receiver, landing, statements)
}

#[test]
fn hybrid_landing_requires_each_dependency_first_admission_before_callback() {
    for missing in 0..2 {
        let (receiver, landing, statements) = landing_receiver();
        let mut bundle = receiver.bundle.clone();
        bundle
            .statements
            .retain(|statement| statement != &statements[missing]);
        let before = receiver.counts();
        let mut called = false;
        let result = ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &bundle.encode_to_vec(),
            &[landing.execution.clone().expect("execution")],
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |_| {
                called = true;
                Ok(())
            },
        );
        assert!(
            matches!(
                result,
                Err(Error::Hybrid(hybrid_codec::Reject::ImportPermission))
            ),
            "missing dependency must reach coverage gate: {:?}",
            result.err()
        );
        assert!(!called);
        assert_eq!(receiver.counts(), before);
        ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &receiver.bundle.encode_to_vec(),
            &[landing.execution.expect("execution")],
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |_| Ok(()),
        )
        .expect("valid landing with independent source/review first admissions");
    }
}
#[test]
fn hybrid_landing_preserves_previously_admitted_dependency_statements_in_both_orders() {
    for landing_first in [false, true] {
        let (mut receiver, landing, statements) = landing_receiver();
        let mut first = receiver.bundle.clone();
        first.landing_witnesses.clear();
        first
            .statements
            .retain(|statement| statement.body.as_ref().expect("body").purpose != 4);
        let originals: Vec<_> = landing
            .source_operation
            .iter()
            .chain(&landing.review_evidence)
            .cloned()
            .collect();
        ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &first.encode_to_vec(),
            &originals,
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |_| Ok(()),
        )
        .expect("independent originals first");
        if landing_first {
            receiver.bundle.statements.rotate_right(1);
        }
        let replicas = ThreadReplica::install_hybrid_import(
            receiver.repo.heddle_dir(),
            &receiver.trust,
            &receiver.bundle.encode_to_vec(),
            &[landing.execution.clone().expect("execution")],
            &receiver.authority,
            &objects::store::InMemoryStore::new(),
            |_| Ok(()),
        )
        .expect("landing after admitted dependencies");
        for (original, statement) in originals.iter().zip(statements) {
            let (_, operation) =
                crypto::import_authority::verify_native_operation(original).expect("op");
            let replica = replicas
                .iter()
                .find(|replica| replica.thread == operation.thread)
                .expect("source replica");
            assert_eq!(
                replica
                    .hosted_admission(operation.id().expect("id"))
                    .expect("retained")
                    .expect("own statement")
                    .statement,
                statement
            );
        }
    }
}

#[test]
fn hybrid_failed_sql_commit_restores_artifacts_from_uncommitted_marker() {
    let receiver = HybridReceiver::new();
    std::fs::write(receiver.repo.heddle_dir().join("commit-pin"), b"old").expect("old");
    let before = receiver.counts();
    let result = receiver.trust.mutate_with_artifacts(
        receiver.bundle.witness_set.as_ref().expect("set"),
        |context| {
            context.sql().execute(
                "INSERT INTO hosted_import_job_keys VALUES(?1,?2)",
                rusqlite::params![[97u8; 32], [97u8; 16]],
            )?;
            context.sql().commit_hook(Some(|| true))?;
            Ok(())
        },
        |_, _| Ok(()),
        |_, writer| {
            writer.write_file(std::path::Path::new("commit-pin"), b"new")?;
            writer.write_file(std::path::Path::new("commit-pack"), b"pack")
        },
        |_, _| Ok(()),
    );
    assert!(
        matches!(result, Err(Error::Sql(_))),
        "SQL commit must really fail"
    );
    assert_eq!(
        std::fs::read(receiver.repo.heddle_dir().join("commit-pin")).expect("restored"),
        b"old"
    );
    assert!(!receiver.repo.heddle_dir().join("commit-pack").exists());
    assert_eq!(receiver.counts(), before);
    receiver
        .trust
        .snapshot()
        .expect("usable after failed commit");
}

#[test]
fn hybrid_installation_process_exit_reconciles_sql_and_artifacts() {
    use std::{path::Path, process::Command};
    for step in [
        "publish",
        "marker",
        "before-commit",
        "commit",
        "done",
        "retired",
    ] {
        let receiver = HybridReceiver::new();
        std::fs::write(receiver.repo.heddle_dir().join("crash-pin"), b"old").expect("old");
        let before = receiver.counts();
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "thread_replication::hosted_trust_tests::hybrid_installation_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("HEDDLE_INSTALL_REPO", receiver.repo.root())
            .env("HEDDLE_INSTALL_CRASH_STEP", step)
            .env("HEDDLE_INSTALL_CRASH_OCCURRENCE", "1")
            .output()
            .expect("child");
        assert_eq!(
            output.status.code(),
            Some(86),
            "{step}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reopened = crate::Repository::open(receiver.repo.root())
            .expect("recover before opening object store");
        let committed = matches!(step, "commit" | "done" | "retired");
        assert_eq!(
            std::fs::read(reopened.heddle_dir().join("crash-pin")).expect("reconciled pin"),
            if committed {
                b"new".as_slice()
            } else {
                b"old".as_slice()
            }
        );
        assert_eq!(reopened.heddle_dir().join("crash-pack").exists(), committed);
        let db = crate::local_metadata::open(reopened.heddle_dir()).expect("reconciled sql");
        assert_eq!(
            db.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
                .expect("replicas"),
            before[0] + i64::from(committed)
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM hosted_import_admissions", [], |r| r
                .get::<_, i64>(
                0
            ))
            .expect("admissions"),
            before[3] + 2 * i64::from(committed)
        );
        assert!(
            !reopened
                .heddle_dir()
                .join(Path::new("hosted-install.intent"))
                .exists()
        );
    }
}
#[test]
#[ignore = "entry point for the public-installer process-exit test"]
fn hybrid_installation_crash_child() {
    let repository =
        crate::Repository::open(std::env::var_os("HEDDLE_INSTALL_REPO").expect("repo"))
            .expect("existing repository");
    let f = fixture();
    let selected = root(&f);
    let authority = Authority::new(&f);
    let trust = HostedTrust::open(
        repository.heddle_dir(),
        &selected.authority,
        TestClock::new(1_350_000),
    )
    .expect("trust");
    let mut bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    bundle.history_proofs = [
        "genesis_proof",
        "genesis_dev_proof",
        "publication_proof",
        "renewed_publication_proof",
    ]
    .map(|name| record(&f, name))
    .to_vec();
    ThreadReplica::install_hybrid_import(
        repository.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &[record(&f, "converted_main")],
        &authority,
        &objects::store::InMemoryStore::new(),
        |writer| {
            if std::env::var_os("HEDDLE_INSTALL_CRASH_INVALID_TOML").is_some() {
                writer.write_file(
                    std::path::Path::new("config.toml"),
                    b"invalid installed config [",
                )?;
            }
            writer.write_file(std::path::Path::new("crash-pin"), b"new")?;
            writer.write_file(std::path::Path::new("crash-pack"), b"pack")
        },
    )
    .expect("install");
    panic!("crash point was not reached");
}

#[test]
fn hybrid_crash_recovers_config_before_repository_readers() {
    let receiver = HybridReceiver::new();
    let before = std::fs::read(receiver.repo.heddle_dir().join("config.toml")).expect("config");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "thread_replication::hosted_trust_tests::hybrid_installation_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("HEDDLE_INSTALL_REPO", receiver.repo.root())
        .env("HEDDLE_INSTALL_CRASH_INVALID_TOML", "1")
        .env("HEDDLE_INSTALL_CRASH_STEP", "publish")
        .env("HEDDLE_INSTALL_CRASH_OCCURRENCE", "1")
        .output()
        .expect("child");
    assert_eq!(
        output.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        std::fs::read(receiver.repo.heddle_dir().join("config.toml")).expect("uncommitted config"),
        before
    );
    crate::Repository::open(receiver.repo.root())
        .expect("undo precedes reading the uncommitted config");
    assert_eq!(
        std::fs::read(receiver.repo.heddle_dir().join("config.toml")).expect("restored config"),
        before
    );
}

#[test]
fn native_and_import_retention_are_exclusive_without_shared_admissions() {
    use super::{delegated_import, native_witness};
    let f = fixture();
    let current: host::SignedHostedWitnessSetV1 = record(&f, "current_set");
    let imported: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    let native = wire::NativePublicProofBundleV1 {
        format_version: 1,
        ..Default::default()
    };
    let thread = objects::object::ContentHash::from_bytes([42; 32]);
    for native_first in [false, true] {
        let dir = tempfile::tempdir().expect("repository");
        let repository = crate::Repository::init_default(dir.path()).expect("init");
        let selected = root(&f);
        select_root(repository.heddle_dir(), &selected).expect("root");
        let trust = HostedTrust::open(
            repository.heddle_dir(),
            &selected.authority,
            TestClock::new(1_100_000),
        )
        .expect("trust");
        // Exercise storage exclusivity directly: neither test relies on the
        // shared first-admission table to reject a cross-arm refresh.
        trust
            .mutate(&current, |c| {
                if native_first {
                    native_witness::retain_bundle(c, thread, &native)
                } else {
                    delegated_import::retain_bundle(c, &thread, &imported)
                }
            })
            .expect("first arm control");
        let db = crate::local_metadata::open(repository.heddle_dir()).expect("db");
        let admissions: i64 = db
            .query_row("SELECT count(*) FROM hosted_import_admissions", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(admissions, 0);
        let result = trust.mutate(&current, |c| {
            if native_first {
                delegated_import::retain_bundle(c, &thread, &imported)
            } else {
                native_witness::retain_bundle(c, thread, &native)
            }
        });
        assert!(
            matches!(result, Err(Error::Hybrid(hybrid_codec::Reject::Scope))),
            "cross-arm retention must reject without shared admissions"
        );
        let counts:(i64,i64)=db.query_row("SELECT (SELECT count(*) FROM hosted_native_proofs),(SELECT count(*) FROM hosted_import_proofs)",[],|r|Ok((r.get(0)?,r.get(1)?))).expect("counts");
        assert_eq!(
            counts,
            if native_first { (1, 0) } else { (0, 1) },
            "refusal preserves the first arm"
        );
    }
}
