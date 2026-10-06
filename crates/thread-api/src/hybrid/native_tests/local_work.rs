use std::cell::Cell;

use repo::thread_replication::{ThreadReplica, hosted_trust::*};

use super::*;

fn install(name: &str, accepted: bool) {
    let bundle: wire::NativePublicProofBundleV1 = record(&fixture(), name);
    // These negatives have valid original signatures, claims, capabilities and
    // witness commitments. Only the native owner/cutoff gate may refuse them.
    verify_semantics(&bundle, 1_100_000).expect("valid portable evidence");
    let originals: Vec<_> = bundle
        .authority_witnesses
        .iter()
        .flat_map(|p| p.dependencies.iter())
        .filter(|record| record.format == objects::object::thread_replication::OPERATION_FORMAT)
        .filter(|record| {
            verify::verify_native_operation(record)
                .expect("original signature")
                .1
                .local_integration()
                .expect("integration codec")
                .is_some()
        })
        .cloned()
        .collect();
    assert!(
        !originals.is_empty(),
        "select the local integration and resolve its source closure"
    );
    install_bundle(
        name,
        bundle,
        &originals,
        (!accepted).then_some(hybrid_codec::Reject::Scope),
    );
}

pub(super) fn install_bundle(
    name: &str,
    bundle: wire::NativePublicProofBundleV1,
    originals: &[wire::SignedRecord],
    rejection: Option<hybrid_codec::Reject>,
) {
    install_bundle_with_refusal(name, bundle, originals, rejection.map(|expected| {
        move |error: &repo::thread_replication::Error| {
            matches!(error, repo::thread_replication::Error::Hybrid(actual) |
                repo::thread_replication::Error::HybridEvidence(crypto::import_authority::Error::Contract(actual))
                if *actual == expected)
        }
    }));
}

pub(super) fn install_bundle_with_refusal(
    name: &str,
    bundle: wire::NativePublicProofBundleV1,
    originals: &[wire::SignedRecord],
    rejection: Option<impl Fn(&repo::thread_replication::Error) -> bool>,
) {
    use crate::hybrid::authority::tests::{bundle as imported, selected};

    let limits =
        heddleco_capability_verifier::VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let pinned = selected(&imported(), limits);
    let history = AcceptedHistory::from_native_spool(&bundle, &pinned, 1100, limits)
        .expect("selected native history");
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
        *history.genesis(),
        *history.initial_owner(),
    )
    .expect("independent Spool");
    let trust =
        HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock).expect("trust");
    let authority = SelectedAuthority::new_native(
        history,
        bundle.clone(),
        |_: &wire::NativePublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let before = super::receiver_snapshot(repository.heddle_dir());
    let published = Cell::new(false);
    let result = ThreadReplica::install_hybrid_native(
        repository.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        originals,
        &authority,
        repository.store(),
        |artifacts| {
            published.set(true);
            artifacts.write_file(std::path::Path::new("local-work-proof"), name.as_bytes())
        },
    );
    if rejection.is_none() {
        let replicas = result.unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert!(published.get());
        assert_eq!(replicas.is_empty(), originals.is_empty());
        for replica in replicas {
            assert_eq!(
                replica.hybrid_native_bundle().expect("retained history"),
                Some(bundle.clone())
            );
        }
    } else {
        assert!(
            result
                .as_ref()
                .is_err_and(|error| rejection.as_ref().is_some_and(|reject| reject(error))),
            "{name}: native authority must reject before installation: {:?}",
            result.err()
        );
        assert!(
            !published.get(),
            "{name}: rejection must precede artifact publication"
        );
        assert!(!repository.heddle_dir().join("local-work-proof").exists());
        assert_eq!(
            super::receiver_snapshot(repository.heddle_dir()),
            before,
            "{name}: rejection must preserve all native and trust state"
        );
    }
}

#[test]
fn native_local_integration_rejects_wrong_key_with_valid_claim() {
    install("local_integration_push", true);
    install("local_integration_wrong_key", false);
}

#[test]
fn native_local_work_rejects_beyond_signed_claim_cutoff() {
    install("local_integration_push", true);
    // A frontier head covers its ancestors, including a local integration.
    install("local_integration_ancestor_push", true);
    install("local_integration_beyond_claim_cutoff", false);
}

#[test]
fn native_local_integration_rejects_wrong_key_beyond_unchanged_claim() {
    install("local_integration_push", true);
    install("local_integration_wrong_key_unchanged_claim", false);
}
