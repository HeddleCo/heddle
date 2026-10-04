use api::{heddle::api::v1alpha2 as wire, hybrid_codec, witness_trust};
use crypto::import_authority::{self as verify, NativeClosure, OriginalGeneses, WitnessEvidence};
use prost::Message;
use repo::thread_replication::delegated_import::AcceptedAuthority;

use super::authority::{AcceptedHistory, SelectedAuthority};

mod admissions;
mod local_work;

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/native-host-witness-v1.json"
    ))
    .expect("alpha.30 native vectors")
}
fn record<T: Message + Default>(f: &serde_json::Value, name: &str) -> T {
    hybrid_codec::strict_decode(
        &hex::decode(f["wire_vectors"][name]["wire_hex"].as_str().expect("wire")).expect("hex"),
        api::import_authority::MAX_BUNDLE_BYTES,
    )
    .expect("canonical vector")
}
fn set(bundle: &wire::NativePublicProofBundleV1, now: i64) -> witness_trust::VerifiedWitnessSet {
    let root = hex::decode(
        fixture()["keys"]["root"]["public_key_hex"]
            .as_str()
            .expect("independent root"),
    )
    .expect("root hex");
    witness_trust::verify_set(
        bundle.witness_set.as_ref().expect("set"),
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: now,
            clock_floor_unix_millis: 1_000_000,
            known_job_keys: &[],
        },
        None,
    )
    .expect("independent root-authenticated set")
}
fn verify_semantics(
    bundle: &wire::NativePublicProofBundleV1,
    now: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    use super::authority::tests::{bundle as imported, selected};
    let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)?;
    let pinned = selected(&imported(), limits);
    let history = AcceptedHistory::from_native_spool(bundle, &pinned, now / 1000, limits)?;
    let authority = SelectedAuthority::new_native(
        history,
        bundle.clone(),
        |_: &wire::NativePublicProofBundleV1,
         _: i64,
         _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
    );
    let fresh = set(bundle, now);
    api::native_witness::verify_bundle_witnesses(bundle, &fresh, now)?;
    let mut originals = Vec::new();
    for p in &bundle.genesis_witnesses {
        originals.extend(p.original_genesis.iter().cloned());
    }
    for p in &bundle.authority_witnesses {
        originals.extend(p.original.iter().cloned());
        originals.extend(p.dependencies.iter().cloned());
    }
    for p in &bundle.landing_witnesses {
        originals.extend(p.execution.iter().cloned());
        originals.extend(p.source_operation.iter().cloned());
        originals.extend(p.review_evidence.iter().cloned());
    }
    let boundaries: Vec<_> = bundle
        .genesis_witnesses
        .iter()
        .filter_map(|p| p.boundary_acceptance.clone())
        .chain(
            bundle
                .authority_witnesses
                .iter()
                .flat_map(|p| p.boundary_acceptances.clone()),
        )
        .collect();
    let closure = NativeClosure::verify_with_boundaries(&originals, &boundaries)?;
    for signed in &bundle.statements {
        let s = signed.body.as_ref().expect("body");
        let proof = bundle
            .history_proofs
            .iter()
            .find(|p| WitnessEvidence::resolve(&fresh, signed, Some(p), false, now).is_ok());
        let evidence = WitnessEvidence::resolve(&fresh, signed, proof, false, now)?;
        let selected = authority.for_witness(s)?;
        let path = selected
            .keyring
            .wire()
            .canonical_spool_path_segments
            .join("/");
        let context = verify::NativeAuthorityContext {
            owner: selected.owner,
            spool_uuid: uuid::Uuid::from_bytes(selected.keyring.owner_genesis().spool_uuid()),
            spool_genesis: selected.spool_genesis_digest,
            transfer_sequence: s.ownership_transfer_sequence,
            spool_path: &path,
            witness_set: &fresh,
            original_geneses: OriginalGeneses::Native(&bundle.genesis_witnesses),
            known_job_associations: &[],
            forbidden_authority_keys: &[],
        };
        match s.purpose {
            1 => {
                let p = bundle
                    .genesis_witnesses
                    .iter()
                    .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                    .expect("genesis payload");
                let binding_selection = authority.for_native_binding(
                    p.binding
                        .as_ref()
                        .expect("binding")
                        .body
                        .as_ref()
                        .expect("binding body"),
                )?;
                crypto::native_witness::verify_genesis_payload(
                    p,
                    &evidence,
                    &binding_selection,
                    &closure,
                    &context,
                    |r| authority.native_revoked(s, r),
                )?;
            }
            2 => {
                let p = bundle
                    .authority_witnesses
                    .iter()
                    .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                    .expect("authority payload");
                verify::verify_authority_payload(p, &evidence, &closure, &context, |r| {
                    authority.native_revoked(s, r)
                })?;
            }
            4 => {
                let p = bundle
                    .landing_witnesses
                    .iter()
                    .find(|p| hybrid_codec::canonical(*p).is_ok_and(|b| b == s.canonical_payload))
                    .expect("landing payload");
                verify::verify_landing_payload(p, &evidence, &closure, &context, |r| {
                    authority.native_revoked(s, r)
                })?;
            }
            _ => panic!("unexpected purpose"),
        }
    }
    Ok(())
}
#[test]
fn native_vectors_verify_full_native_models_and_authority() {
    let f = fixture();
    for name in f["positive"].as_array().expect("controls") {
        let name = name.as_str().expect("name");
        let b: wire::NativePublicProofBundleV1 = record(&f, name);
        let now = if name == "retired_start_thread" {
            1_350_000
        } else {
            1_100_000
        };
        verify_semantics(&b, now).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }
}
#[test]
fn native_carrier_rejections_have_exact_passing_controls() {
    let f = fixture();
    for negative in f["negative"].as_array().expect("negatives") {
        let id = negative["id"].as_str().expect("id");
        let control = negative["control"].as_str().expect("control");
        let gate = negative["gate"].as_str().expect("gate");
        if gate == "import" || gate == "dispatch" {
            continue;
        }
        let passing: wire::NativePublicProofBundleV1 = record(&f, control);
        let failing: wire::NativePublicProofBundleV1 = record(&f, id);
        let now = if control == "retired_start_thread" {
            1_350_000
        } else {
            1_100_000
        };
        api::native_witness::verify_bundle_witnesses(&passing, &set(&passing, now), now)
            .expect("nearby passing control");
        let result = if gate == "witness" {
            api::native_witness::verify_bundle_witnesses(&failing, &set(&passing, now), now)
        } else {
            api::native_witness::validate_public_bundle(&failing)
        };
        assert!(result.is_err(), "{id} must reject");
    }
}
#[test]
fn native_transport_requires_explicit_arm_and_negotiation() {
    let b: wire::NativePublicProofBundleV1 = record(&fixture(), "start_thread");
    let open = wire::ReplicationOpen {
        native_authority: Some(b.clone()),
        protocol: Some(super::protocol()),
        ..Default::default()
    };
    super::replication_open(&open).expect("native control");
    let mut denied = open.clone();
    denied.protocol = None;
    assert!(super::replication_open(&denied).is_err());
    denied = open.clone();
    denied.import_authority = Some(super::authority::tests::bundle());
    assert!(super::replication_open(&denied).is_err());
    super::replication_open(&open).expect("control remains valid");
}

#[tokio::test]
async fn native_start_thread_binding_requires_protocol_before_request_proof() {
    use api::v2::client::Rpc;

    use crate::{credentials::Credentials, rpc, transport::Authorize};

    let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "start_thread");
    let payload = &bundle.genesis_witnesses[0];
    let mut request = wire::StartThreadRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        thread_genesis: payload.original_genesis.clone(),
        creator_authority: payload.creator_authority_envelope.clone(),
        native_genesis_authority: payload.binding.clone(),
        ..Default::default()
    };
    let context = Credentials::Public
        .context(
            rpc::ThreadServiceStartThread::METHOD,
            &request.encode_to_vec(),
        )
        .await
        .expect("complete creator-bound request");
    assert_eq!(context.protocol, Some(super::protocol()));
    request.creator_authority.push(0);
    assert!(
        Credentials::Public
            .context(
                rpc::ThreadServiceStartThread::METHOD,
                &request.encode_to_vec()
            )
            .await
            .is_err(),
        "a substituted envelope must reject before request signing"
    );
    request.creator_authority = payload.creator_authority_envelope.clone();
    let context = Credentials::Public
        .context(
            rpc::ThreadServiceStartThread::METHOD,
            &request.encode_to_vec(),
        )
        .await
        .expect("nearby exact-envelope control");
    assert_eq!(context.protocol, Some(super::protocol()));
}

#[test]
fn native_and_import_carriers_have_no_delegation_or_dispatch_fallback() {
    let f = fixture();
    let native: wire::NativePublicProofBundleV1 = record(&f, "start_thread");
    let imported: wire::ImportPublicProofBundleV1 = record(&f, "import_complete");
    super::bundles(None, Some(&native)).expect("native control");
    super::bundles(Some(&imported), None).expect("delegated import control");
    let missing: wire::ImportPublicProofBundleV1 = record(&f, "import_without_delegation");
    assert!(
        super::bundles(Some(&missing), None).is_err(),
        "an import without its delegation must reject"
    );
    assert!(
        super::bundles(Some(&imported), Some(&native)).is_err(),
        "dual carriers must reject"
    );
    let native_as_import = hybrid_codec::strict_decode::<wire::ImportPublicProofBundleV1>(
        &native.encode_to_vec(),
        api::import_authority::MAX_BUNDLE_BYTES,
    );
    assert!(
        native_as_import.is_err()
            || native_as_import.is_ok_and(|b| super::bundles(Some(&b), None).is_err()),
        "native history cannot enter the import arm"
    );
    let import_as_native = hybrid_codec::strict_decode::<wire::NativePublicProofBundleV1>(
        &imported.encode_to_vec(),
        api::import_authority::MAX_BUNDLE_BYTES,
    );
    assert!(
        import_as_native.is_err()
            || import_as_native.is_ok_and(|b| super::bundles(None, Some(&b)).is_err()),
        "import history cannot enter the native arm"
    );
    super::bundles(None, Some(&native)).expect("native control after negatives");
    super::bundles(Some(&imported), None).expect("import control after negatives");
}

struct ReceiverClock;
impl repo::thread_replication::hosted_trust::Clock for ReceiverClock {
    fn now_millis(&self) -> repo::thread_replication::Result<i64> {
        Ok(1_100_000)
    }
    fn elapsed_millis(&self) -> repo::thread_replication::Result<u64> {
        Ok(0)
    }
}

#[test]
fn native_install_retains_exact_genesis_claim_landing_and_capture_history() {
    use repo::thread_replication::{ThreadReplica, hosted_trust::*};

    use super::authority::tests::{bundle as imported, selected};
    for name in [
        "start_thread",
        "account_source",
        "local_adopt_push",
        "ownership_resolution",
        "native_metadata",
        "native_landing",
        "post_landing_capture",
        "boundary_acceptance",
        "local_integration_push",
        "local_integration_ancestor_push",
        "distinct_owner_chains",
    ] {
        let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), name);
        let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
            .expect("limits");
        let pinned = selected(&imported(), limits);
        let history = AcceptedHistory::from_native_spool(&bundle, &pinned, 1100, limits)
            .expect("selected lineage");
        let genesis = *history.genesis();
        let owner = *history.initial_owner();
        let authority = SelectedAuthority::new_native(
            history,
            bundle.clone(),
            |_: &wire::NativePublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
        );
        let directory = tempfile::tempdir().expect("fresh receiver");
        let repository = repo::Repository::init_default(directory.path()).expect("repository");
        let root = RootSelection {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: hex::decode(
                fixture()["keys"]["root"]["public_key_hex"]
                    .as_str()
                    .expect("root"),
            )
            .expect("root bytes")
            .try_into()
            .expect("root key"),
        };
        select_root(repository.heddle_dir(), &root).expect("independent root");
        select_spool(
            repository.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            genesis,
            owner,
        )
        .expect("independent Spool");
        let trust = HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock)
            .expect("trust");
        let records: Vec<_> = bundle
            .genesis_witnesses
            .iter()
            .filter_map(|p| p.original_genesis.clone())
            .chain(
                bundle
                    .authority_witnesses
                    .iter()
                    .filter_map(|p| p.original.clone()),
            )
            .chain(
                bundle
                    .landing_witnesses
                    .iter()
                    .filter_map(|p| p.execution.clone()),
            )
            .collect();
        eprintln!("installing native control {name}");
        let replicas = ThreadReplica::install_hybrid_native(
            repository.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &authority,
            repository.store(),
            |artifacts| {
                artifacts.write_file(
                    std::path::Path::new("native-install-proof"),
                    name.as_bytes(),
                )
            },
        )
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(!replicas.is_empty(), "{name}");
        for replica in &replicas {
            assert_eq!(
                replica
                    .hybrid_native_bundle()
                    .expect("retained public history"),
                Some(bundle.clone())
            );
            let wrapper = replica.genesis_record().expect("retained original");
            let payload = bundle
                .genesis_witnesses
                .iter()
                .find(|p| p.original_genesis == wrapper.genesis)
                .expect("exact genesis");
            assert_eq!(wrapper.native_genesis_authority, payload.binding);
            assert_eq!(
                wrapper.creator_authority,
                payload.creator_authority_envelope
            );
        }
        eprintln!("reinstalling native control {name}");
        // A second install rechecks durable trust and preserves the first admission.
        ThreadReplica::install_hybrid_native(
            repository.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &authority,
            repository.store(),
            |_| Ok(()),
        )
        .expect("idempotent verified history");
    }
}

#[test]
fn native_commit_rechecks_current_authority_and_restores_artifacts_and_witnesses() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use repo::thread_replication::{ThreadReplica, hosted_trust::*};

    use super::authority::tests::{bundle as imported, selected};

    let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), "account_source");
    for revoke_during_publication in [false, true] {
        let limits = heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60)
            .expect("limits");
        let pinned = selected(&imported(), limits);
        let history = AcceptedHistory::from_native_spool(&bundle, &pinned, 1100, limits)
            .expect("independently selected history");
        let directory = tempfile::tempdir().expect("fresh receiver");
        let repository = repo::Repository::init_default(directory.path()).expect("repository");
        let root = RootSelection {
            authority: "https://weft.example.test".into(),
            root_id: "descriptor-root-1".into(),
            public_key: hex::decode(
                fixture()["keys"]["root"]["public_key_hex"]
                    .as_str()
                    .expect("root"),
            )
            .expect("root bytes")
            .try_into()
            .expect("root key"),
        };
        select_root(repository.heddle_dir(), &root).expect("selected root");
        select_spool(
            repository.heddle_dir(),
            pinned.owner_genesis().spool_uuid(),
            *history.genesis(),
            *history.initial_owner(),
        )
        .expect("selected Spool");
        let trust = HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock)
            .expect("trust");
        let revoked = AtomicBool::new(false);
        let authority = SelectedAuthority::new_native(
            history,
            bundle.clone(),
            |_: &wire::NativePublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| {
                if revoked.load(Ordering::SeqCst) {
                    Err(hybrid_codec::Reject::StaleContext.into())
                } else {
                    Ok(())
                }
            },
        );
        let records: Vec<_> = bundle
            .genesis_witnesses
            .iter()
            .filter_map(|p| p.original_genesis.clone())
            .chain(
                bundle
                    .authority_witnesses
                    .iter()
                    .filter_map(|p| p.original.clone()),
            )
            .collect();
        let result = ThreadReplica::install_hybrid_native(
            repository.heddle_dir(),
            &trust,
            &bundle.encode_to_vec(),
            &records,
            &authority,
            repository.store(),
            |artifacts| {
                artifacts.write_file(std::path::Path::new("native-commit-proof"), b"published")?;
                revoked.store(revoke_during_publication, Ordering::SeqCst);
                Ok(())
            },
        );
        if revoke_during_publication {
            assert!(
                result.is_err(),
                "native commit authority must reject after publication and restore every artifact"
            );
            assert!(
                !repository.heddle_dir().join("native-commit-proof").exists(),
                "rejected native artifact must roll back"
            );
            let db = repo::local_metadata::open(repository.heddle_dir()).expect("metadata");
            for table in [
                "hosted_native_proofs",
                "hosted_native_genesis_bindings",
                "hosted_import_admissions",
            ] {
                let count: i64 = db
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .expect("retained history count");
                assert_eq!(count, 0, "rejected native mutation must not retain {table}");
            }
        } else {
            let replicas = result.expect("nearby authorized native publication");
            assert!(!replicas.is_empty());
            assert_eq!(
                std::fs::read(repository.heddle_dir().join("native-commit-proof"))
                    .expect("committed artifact"),
                b"published"
            );
            assert_eq!(
                replicas[0]
                    .hybrid_native_bundle()
                    .expect("committed witness"),
                Some(bundle.clone())
            );
        }
    }
}
