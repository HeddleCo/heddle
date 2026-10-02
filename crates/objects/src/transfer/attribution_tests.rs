// SPDX-License-Identifier: Apache-2.0
use super::graph::*;
use crate::{
    object::{
        Attribution, AttributionBasis, AttributionClaim, AttributionEvidenceV1, AttributionSource,
        Blob, Principal, State, Tree,
    },
    store::{InMemoryStore, ObjectStore},
};

fn fixture() -> (InMemoryStore, State, Blob) {
    let store = InMemoryStore::new();
    let tree = Tree::new();
    store.put_tree(&tree).expect("tree");
    let evidence = AttributionEvidenceV1 {
        harness: Some(AttributionClaim::new(
            "codex",
            AttributionBasis::Observed,
            AttributionSource::Process,
        )),
        ..Default::default()
    }
    .to_blob()
    .expect("evidence");
    let state = State::new(
        tree.hash(),
        vec![],
        Attribution::human(Principal::new("Test", "test@example.test")),
    )
    .with_attribution_evidence(evidence.hash());
    store.put_state(&state).expect("state");
    (store, state, evidence)
}

#[test]
fn source_attribution_evidence_is_required_in_all_transfer_plans() {
    let (store, state, evidence) = fixture();
    assert!(enumerate_state_closure(&store, state.id()).is_err());
    assert!(enumerate_state_closure_plan(&store, state.id()).is_err());
    assert!(
        enumerate_state_closure_transfer_from_boundaries(&store, state.id(), &[], 100).is_err()
    );
    store.put_blob(&evidence).expect("evidence");
    let objects = enumerate_state_closure(&store, state.id()).expect("closure");
    assert_eq!(objects.len(), 3);
    assert!(
        objects
            .iter()
            .any(|object| object.id == ObjectId::Hash(evidence.hash())
                && object.obj_type == ObjectType::Blob)
    );
    let receiver = InMemoryStore::new();
    receiver
        .put_state(&state)
        .expect("received source metadata");
    receiver.put_tree(&Tree::new()).expect("received tree");
    let missing =
        super::availability::plan_object_availability(&receiver, &objects).expect("availability");
    assert_eq!(
        missing.want_objects,
        vec![ObjectId::Hash(evidence.hash())],
        "metadata does not hide the missing required Blob"
    );
    assert!(!missing.is_complete());
    let plan = enumerate_state_closure_transfer_from_boundaries(&store, state.id(), &[], 100)
        .expect("transfer");
    assert_eq!(plan.planned_objects.len(), 3);
    assert_eq!(plan.full_objects.expect("full descriptors").len(), 3);
}

#[test]
fn source_attribution_evidence_is_validated_even_if_already_excluded() {
    let (store, parent, evidence) = fixture();
    store.put_blob(&evidence).expect("evidence");
    let child = State::new(parent.tree, vec![parent.id()], parent.attribution.clone())
        .with_attribution_evidence(evidence.hash());
    store.put_state(&child).expect("child");
    let options = StateClosureOptions {
        depth: None,
        exclude_states: vec![parent.id()],
    };
    let objects = enumerate_state_closure_with_options(&store, child.id(), options)
        .expect("incremental closure");
    assert_eq!(
        objects.len(),
        1,
        "receiver already has the parent's required evidence"
    );
    let bad = Blob::new(b"ordinary blob is not typed attribution evidence".to_vec());
    store.put_blob(&bad).expect("bad blob");
    let invalid = State::new(parent.tree, vec![], parent.attribution.clone())
        .with_attribution_evidence(bad.hash());
    store
        .put_state(&invalid)
        .expect("invalid state metadata remains storable");
    assert!(enumerate_state_closure_plan(&store, invalid.id()).is_err());
    let invalid_child = State::new(
        invalid.tree,
        vec![invalid.id()],
        invalid.attribution.clone(),
    )
    .with_attribution_evidence(bad.hash());
    store
        .put_state(&invalid_child)
        .expect("invalid child metadata");
    assert!(
        enumerate_state_closure_with_options(
            &store,
            invalid_child.id(),
            StateClosureOptions {
                depth: None,
                exclude_states: vec![invalid.id()],
            }
        )
        .is_err(),
        "excluded blob addresses cannot skip evidence validation for a new State"
    );
}
