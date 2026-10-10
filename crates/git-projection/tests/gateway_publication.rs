#![cfg(feature = "gateway-publication")]
//! Actual native pack bytes through the shared receiver, without live authority.
#[path = "gateway_publication/support.rs"]
mod support;
use heddle_git_projection::gateway_publication::{HistoryBudget, PreparedHistory};
use objects::{
    object::original_boundary_acceptance::BoundaryOriginalKind,
    store::{FsStore, ObjectStore},
};
use support::*;

#[tokio::test]
async fn every_historical_revision_is_hydrated_and_receiver_checks_actual_bytes() {
    let f = fixture(1024);
    let mut plan = PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget::default(),
        |_| Ok(()),
    )
    .expect("offline exact history");
    assert_eq!(plan.tip(), f.states[1].id());
    assert_eq!(plan.revisions().len(), 2);
    let acceptor = crypto::Ed25519Signer::from_seed(&[62; 32]).expect("explicit fixture acceptor");
    let destination = tempfile::tempdir().expect("fresh native destination");
    let restored = FsStore::new(destination.path());
    restored.init().expect("store");
    for revision in plan.revisions_mut() {
        let before = revision.publication().originals().operations.clone();
        revision
            .sign_acceptance(author(), [BoundaryOriginalKind::Source].into(), &acceptor)
            .expect("explicit acceptance");
        let validated = revision
            .validate_received(received(revision.source()).await, f.spool_genesis())
            .expect("actual standalone receiver validation");
        assert_eq!(
            validated.artifacts().state().id(),
            revision.source().revision()
        );
        assert_eq!(validated.acceptances().proposed().len(), 1);
        for (old, new) in before
            .iter()
            .zip(&revision.publication().originals().operations)
        {
            assert_eq!(
                old.operations, new.operations,
                "acceptance cannot rewrite original signatures"
            );
        }
        if revision.source().revision() == f.states[1].id() {
            let only_tip = tempfile::tempdir().expect("tip-only receiver");
            let tip_store = FsStore::new(only_tip.path());
            tip_store.init().expect("tip store");
            let paths = validated.artifacts().artifact_paths();
            let copy = tempfile::tempdir().expect("tip-only copied transfer");
            let copies = [
                copy.path().join("source.pack"),
                copy.path().join("source.idx"),
            ];
            for (source, target) in paths.iter().zip(&copies) {
                std::fs::copy(source, target).expect("retain original verified transfer");
            }
            tip_store
                .install_pack_streaming(&copies[0], &copies[1])
                .expect("tip bytes");
            assert!(
                tip_store
                    .get_blob(&f.blobs[0].hash())
                    .expect("old content")
                    .is_none(),
                "a selected-tip pack is not full history"
            );
        }
        let paths = validated.artifacts().artifact_paths();
        restored
            .install_pack_streaming(&paths[0], &paths[1])
            .expect("verified closure hydration");
    }
    for state in &f.states {
        assert_eq!(
            restored
                .get_state(&state.id())
                .expect("State")
                .expect("hydrated")
                .encode_current_msgpack()
                .expect("bytes"),
            state.encode_current_msgpack().expect("original bytes")
        );
    }
    for blob in &f.blobs {
        assert_eq!(
            restored
                .get_blob(&blob.hash())
                .expect("blob")
                .expect("historical bytes")
                .content(),
            blob.content()
        );
    }
}

#[tokio::test]
async fn tampered_received_artifact_is_rejected_instead_of_using_sender_storage() {
    let f = fixture(50);
    let plan = PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget::default(),
        |_| Ok(()),
    )
    .expect("prepare");
    let revision = &plan.revisions()[0];
    let directory = received(revision.source()).await;
    std::fs::write(directory.path().join("source.pack"), b"corrupt").expect("tamper upload");
    assert!(
        revision
            .validate_received(directory, f.spool_genesis())
            .is_err()
    );
}

#[test]
fn repeat_preparation_has_exact_stable_per_revision_request_identities() {
    let f = fixture(50);
    let make = || {
        PreparedHistory::prepare(
            &remote(),
            &f.store,
            f.selection(),
            f.scope(),
            f.root.path(),
            HistoryBudget::default(),
            |_| Ok(()),
        )
        .expect("prepare")
    };
    let first = make();
    let second = make();
    for (a, b) in first.revisions().iter().zip(second.revisions()) {
        assert_eq!(a.publication().opening(), b.publication().opening());
    }
    assert_ne!(
        first.revisions()[0]
            .publication()
            .opening()
            .client_operation_id,
        first.revisions()[1]
            .publication()
            .opening()
            .client_operation_id
    );
}

#[test]
fn cumulative_history_bound_and_final_disclosure_recheck_fail_closed() {
    let f = fixture(4096);
    let result = PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget {
            decoded_bytes: 6000,
            ..HistoryBudget::default()
        },
        |_| Ok(()),
    );
    assert!(
        result.is_err(),
        "two individually bounded sources must share one decoded budget"
    );
    let calls = std::cell::Cell::new(0);
    let result = PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget::default(),
        |states| {
            assert_eq!(states.len(), 2);
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                Err(heddle_git_projection::GitProjectionError::Git(
                    "current historical visibility revoked".into(),
                ))
            } else {
                Ok(())
            }
        },
    );
    assert!(
        result
            .err()
            .expect("revoked")
            .to_string()
            .contains("visibility revoked")
    );
    assert_eq!(calls.get(), 2);
    assert_eq!(
        std::fs::read_dir(f.root.path()).expect("scratch").count(),
        1,
        "failed plans remove all temporary source packs"
    );
}

#[test]
fn absent_ancestor_or_changed_unhashed_state_metadata_is_rejected() {
    let mut f = fixture(50);
    let ancestor = f.originals.remove(0);
    assert!(
        PreparedHistory::prepare(
            &remote(),
            &f.store,
            f.selection(),
            f.scope(),
            f.root.path(),
            HistoryBudget::default(),
            |_| Ok(())
        )
        .is_err()
    );
    f.originals.insert(0, ancestor);
    let mut changed = f.states[0].clone();
    changed.git_lossy = !changed.git_lossy;
    assert_eq!(changed.id(), f.states[0].id());
    f.store
        .put_state(&changed)
        .expect("substitute unhashed metadata");
    assert!(
        PreparedHistory::prepare(
            &remote(),
            &f.store,
            f.selection(),
            f.scope(),
            f.root.path(),
            HistoryBudget::default(),
            |_| Ok(())
        )
        .is_err()
    );
}

#[test]
fn prepared_history_is_causal_order_when_selected_tip_hash_sorts_before_parent() {
    use crypto::thread_operation::SignedOperation;
    use objects::object::{
        ChangeId,
        thread_replication::{AuthoredCapture, ThreadOperationBody},
    };
    let mut f = fixture(35);
    let timestamp =
        chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("fixed fixture timestamp");
    let parent = f.states[0]
        .clone()
        .with_change_id(ChangeId::from_bytes([1; 16]))
        .with_timestamp(timestamp);
    let mut child = f.states[1]
        .clone()
        .with_change_id(ChangeId::from_bytes([2; 16]))
        .with_timestamp(timestamp);
    child.parents = vec![parent.id()];
    let mut found = false;
    // Fixed test vectors, not probabilistic timing: choose a stable hash-order inversion.
    for nonce in 0..4096 {
        child.intent = Some(format!("topological-order-{nonce}"));
        if child.id() < parent.id() {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "deterministic fixture has a tip-before-parent hash ordering"
    );
    let signer = crypto::Ed25519Signer::from_seed(&[61; 32]).expect("fixture signer");
    let mut first = f.originals[0].verify().expect("original parent");
    first.body = ThreadOperationBody::Capture(AuthoredCapture {
        result: parent
            .encode_current_msgpack()
            .expect("parent bytes")
            .into(),
        author: author(),
    });
    let first_id = first.id().expect("parent operation");
    let mut last = f.originals[1].verify().expect("original tip");
    last.parents = [first_id].into();
    last.body = ThreadOperationBody::Capture(AuthoredCapture {
        result: child.encode_current_msgpack().expect("tip bytes").into(),
        author: author(),
    });
    f.store.put_state(&parent).expect("store parent");
    f.store.put_state(&child).expect("store tip");
    f.states = vec![parent, child];
    // The caller's signed-original transport order is also irrelevant.
    f.originals = vec![
        SignedOperation::sign(&last, &signer).expect("signed tip"),
        SignedOperation::sign(&first, &signer).expect("signed parent"),
    ];
    let plan = PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget::default(),
        |_| Ok(()),
    )
    .expect("ordinary native history preparation");
    assert_eq!(
        plan.revisions()
            .iter()
            .map(|r| r.source().revision())
            .collect::<Vec<_>>(),
        vec![f.states[0].id(), f.states[1].id()]
    );
    assert_eq!(
        plan.revisions()
            .last()
            .expect("tip pack")
            .source()
            .revision(),
        plan.tip()
    );
}
