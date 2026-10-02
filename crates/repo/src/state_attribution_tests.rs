// SPDX-License-Identifier: Apache-2.0
use super::*;
use objects::{
    object::{
        Attribution, AttributionBasis, AttributionClaim, AttributionFileChange,
        AttributionOperation, AttributionOperationResolution, AttributionSource, Blob, ContentHash,
        Principal,
    },
    store::{InMemoryStore, ObjectStore},
};

fn evidence() -> AttributionEvidenceV1 {
    AttributionEvidenceV1 {
        harness: Some(AttributionClaim::new(
            "codex",
            AttributionBasis::Observed,
            AttributionSource::Process,
        )),
        ..Default::default()
    }
}

fn state(evidence: &AttributionEvidenceV1) -> State {
    let blob = evidence.to_blob().expect("evidence");
    let attribution = Attribution {
        principal: Principal::new("Test", "test@example.test"),
        agent: evidence.legacy_agent(),
    };
    State::new(ContentHash::compute(b"tree"), vec![], attribution)
        .with_attribution_evidence(blob.hash())
}

#[test]
fn exact_state_evidence_is_required_and_partial_identity_is_agent_authored() {
    let store = InMemoryStore::new();
    let evidence = evidence();
    let state = state(&evidence);
    assert!(matches!(
        load_attribution_evidence(&store, &state),
        Err(HeddleError::MissingObject { .. })
    ));
    store
        .put_blob(&evidence.to_blob().expect("blob"))
        .expect("put");
    let loaded = load_attribution_evidence(&store, &state)
        .expect("load")
        .expect("evidence");
    assert_eq!(loaded, evidence);
    assert!(state.is_agent_authored());
    assert!(state.attribution.agent.is_none());
    assert_eq!(
        attribution_agent_label(None, Some(&loaded)).as_deref(),
        Some("codex (model unknown)")
    );
}

#[test]
fn legacy_states_do_not_acquire_generated_identity() {
    let store = InMemoryStore::new();
    let state = State::new(
        ContentHash::compute(b"tree"),
        vec![],
        Attribution::human(Principal::new("Test", "test@example.test")),
    );
    assert!(
        load_attribution_evidence(&store, &state)
            .expect("legacy")
            .is_none()
    );
    assert_eq!(attribution_agent_label(None, None), None);
    assert!(!state.is_agent_authored());
}

#[test]
fn display_prefers_response_model_without_inventing_response_provider() {
    let mut evidence = evidence();
    evidence.selected.provider = Some(AttributionClaim::new(
        "router",
        AttributionBasis::Configured,
        AttributionSource::Configuration,
    ));
    evidence.selected.model = Some(AttributionClaim::new(
        "auto",
        AttributionBasis::RequestReported,
        AttributionSource::Request,
    ));
    evidence.response.model = Some(AttributionClaim::new(
        "actual-model",
        AttributionBasis::ResponseReported,
        AttributionSource::Response,
    ));
    let state = state(&evidence);
    let legacy = state.attribution.agent.as_ref();
    assert_eq!(legacy.expect("complete selection").model, "auto");
    assert_eq!(
        attribution_model(legacy, Some(&evidence)),
        Some("actual-model")
    );
    assert_eq!(
        attribution_agent_label(legacy, Some(&evidence)).as_deref(),
        Some("codex (actual-model)")
    );
}

#[test]
fn corrupt_required_evidence_never_becomes_human() {
    let store = InMemoryStore::new();
    let blob = Blob::new(b"invalid evidence".to_vec());
    store.put_blob(&blob).expect("put");
    let state = state(&evidence()).with_attribution_evidence(blob.hash());
    assert!(matches!(
        load_attribution_evidence(&store, &state),
        Err(HeddleError::InvalidObject(_))
    ));
}

#[test]
fn contradictory_legacy_agent_is_rejected() {
    let store = InMemoryStore::new();
    let evidence = evidence();
    store
        .put_blob(&evidence.to_blob().expect("blob"))
        .expect("put");
    let mut state = state(&evidence);
    state.attribution.agent = Some(Agent::new("provider", "model"));
    assert!(matches!(
        load_attribution_evidence(&store, &state),
        Err(HeddleError::InvalidObject(_))
    ));
}

#[test]
fn history_model_filter_uses_response_model_even_when_legacy_projects_selection() {
    let store = InMemoryStore::new();
    let mut evidence = evidence();
    evidence.selected.provider = Some(AttributionClaim::new(
        "router",
        AttributionBasis::Configured,
        AttributionSource::Configuration,
    ));
    evidence.selected.model = Some(AttributionClaim::new(
        "auto",
        AttributionBasis::Configured,
        AttributionSource::Configuration,
    ));
    evidence.response.model = Some(AttributionClaim::new(
        "actual-model",
        AttributionBasis::ResponseReported,
        AttributionSource::Response,
    ));
    store
        .put_blob(&evidence.to_blob().expect("blob"))
        .expect("put");
    let state = state(&evidence);
    store.put_state(&state).expect("state");
    let query = crate::HistoryQuery::new(Some(state.id()))
        .with_limit(10)
        .with_agent_filter(Some("actual-model".to_string()));
    let found = crate::query_history_from_source(&store, &query).expect("query");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id(), state.id());
    let selected = crate::HistoryQuery::new(Some(state.id()))
        .with_limit(10)
        .with_agent_filter(Some("auto".to_string()));
    assert!(
        crate::query_history_from_source(&store, &selected)
            .expect("query")
            .is_empty()
    );
}

fn selected_claim(value: &str) -> AttributionClaim {
    AttributionClaim::new(
        value,
        AttributionBasis::RequestReported,
        AttributionSource::Request,
    )
}

fn bound_operation(tool: &str, model: Option<&str>) -> AttributionOperation {
    let mut identity = evidence().operation_identity();
    identity.selected.provider = Some(selected_claim("router"));
    identity.selected.model = model.map(selected_claim);
    identity.scope.actor_id = Some("worker".into());
    identity.scope.harness_session_id = Some("session".into());
    identity.scope.tool_call_id = Some(tool.into());
    AttributionOperation {
        identity,
        changes: vec![AttributionFileChange {
            path: "src/file.rs".into(),
            before: Some(Blob::new(b"before".to_vec()).hash()),
            after: Some(Blob::new(b"after".to_vec()).hash()),
        }],
        resolution: AttributionOperationResolution::ContentBound,
    }
}

fn loaded_operation_evidence(operations: Vec<AttributionOperation>) -> AttributionEvidenceV1 {
    let store = InMemoryStore::new();
    let mut evidence = evidence();
    evidence.harness = Some(selected_claim("capture-harness"));
    evidence.selected.provider = Some(selected_claim("capture-provider"));
    evidence.selected.model = Some(selected_claim("capture-model"));
    evidence.operations = operations;
    let state = state(&evidence);
    assert!(state.is_agent_authored());
    assert!(state.attribution.agent.is_none());
    store
        .put_blob(&evidence.to_blob().expect("operation evidence"))
        .expect("put");
    load_attribution_evidence(&store, &state)
        .expect("load")
        .expect("evidence")
}

#[test]
fn bound_sequential_multi_model_contributors_never_become_the_capture_model() {
    let mut first = bound_operation("tool-1", Some("model-one"));
    let mut second = bound_operation("tool-2", Some("model-two"));
    let intermediate = Blob::new(b"intermediate".to_vec()).hash();
    first.changes[0].after = Some(intermediate);
    second.changes[0].before = Some(intermediate);
    let evidence = loaded_operation_evidence(vec![first, second]);
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some(
            "agent-assisted (mixed attribution): codex (router/model-one); codex (router/model-two)"
        )
    );
}

#[test]
fn sparse_operation_identity_stays_agent_assisted_with_unknown_model() {
    let known = bound_operation("tool-1", Some("model-one"));
    let unknown = bound_operation("tool-2", None);
    let evidence = loaded_operation_evidence(vec![known, unknown.clone()]);
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some("agent-assisted (mixed attribution): codex (router/model-one); codex (model unknown)")
    );
    let evidence = loaded_operation_evidence(vec![unknown]);
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some("agent-assisted: codex (model unknown)")
    );
}

#[test]
fn unresolved_operation_is_a_claim_and_never_a_singular_model() {
    let mut operation = bound_operation("tool-1", Some("reported-model"));
    operation.resolution = AttributionOperationResolution::Unresolved;
    let evidence = loaded_operation_evidence(vec![operation]);
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some("agent-assisted: unresolved claim: codex (router/reported-model)")
    );
}

#[test]
fn homogeneous_contributor_uses_each_response_without_inventing_its_provider() {
    let mut first = bound_operation("tool-1", Some("selected-alias"));
    first.identity.response.model = Some(AttributionClaim::new(
        "response-model",
        AttributionBasis::ResponseReported,
        AttributionSource::Response,
    ));
    first.identity.scope.request_id = Some("request-1".into());
    first.identity.scope.response_id = Some("response-1".into());
    let mut second = first.clone();
    second.identity.scope.tool_call_id = Some("tool-2".into());
    second.identity.scope.request_id = Some("request-2".into());
    second.identity.scope.response_id = Some("response-2".into());
    second
        .identity
        .response
        .model
        .as_mut()
        .expect("model")
        .observation_id = Some("observation-2".into());
    let evidence = loaded_operation_evidence(vec![first, second]);
    assert_eq!(
        attribution_model(None, Some(&evidence)),
        Some("response-model")
    );
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some("agent-assisted: codex (response-model)")
    );
}

#[test]
fn same_model_does_not_collapse_distinct_contributor_namespaces() {
    let first = bound_operation("tool-1", Some("same-model"));
    let mut other_harness = first.clone();
    other_harness.identity.harness = Some(selected_claim("other-harness"));
    let mut other_provider = first.clone();
    other_provider.identity.selected.provider = Some(selected_claim("other-provider"));
    let mut other_actor = first.clone();
    other_actor.identity.scope.actor_id = Some("other-worker".into());
    let mut unknown_actor = first.clone();
    unknown_actor.identity.scope.actor_id = None;
    let mut other_session = first.clone();
    other_session.identity.scope.harness_session_id = Some("other-session".into());
    for mut second in [
        other_harness,
        other_provider,
        other_actor,
        unknown_actor,
        other_session,
    ] {
        second.identity.scope.tool_call_id = Some("tool-2".into());
        let evidence = loaded_operation_evidence(vec![first.clone(), second]);
        assert_eq!(attribution_model(None, Some(&evidence)), None);
        let label = attribution_agent_label(None, Some(&evidence)).expect("agent-assisted");
        assert!(label.starts_with("agent-assisted (mixed attribution): "));
        assert!(!label.contains("capture-"));
    }
}

#[test]
fn incomplete_operations_prevent_singular_attribution_even_without_retained_operations() {
    let mut evidence = loaded_operation_evidence(vec![bound_operation("tool-1", Some("model"))]);
    evidence.operations_incomplete = true;
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some("agent-assisted (incomplete attribution): codex (router/model)")
    );
    evidence.operations.clear();
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some("agent-assisted (incomplete attribution; model unknown)")
    );
}

#[test]
fn history_model_filter_does_not_pick_the_last_or_capture_model_for_mixed_operations() {
    let store = InMemoryStore::new();
    let evidence = loaded_operation_evidence(vec![
        bound_operation("tool-1", Some("model-one")),
        bound_operation("tool-2", Some("model-two")),
    ]);
    store
        .put_blob(&evidence.to_blob().expect("blob"))
        .expect("put");
    let state = state(&evidence);
    store.put_state(&state).expect("state");
    for model in ["model-one", "model-two", "capture-model"] {
        let query = crate::HistoryQuery::new(Some(state.id()))
            .with_limit(10)
            .with_agent_filter(Some(model.to_string()));
        assert!(
            crate::query_history_from_source(&store, &query)
                .expect("query")
                .is_empty()
        );
    }
}

#[test]
fn contributor_summary_caps_unique_identities_without_hiding_unresolved_activity() {
    let first = bound_operation("tool-1", Some("model-one"));
    let mut repeated = first.clone();
    repeated.identity.scope.tool_call_id = Some("tool-1-again".into());
    let second = bound_operation("tool-2", Some("model-two"));
    let third = bound_operation("tool-3", Some("model-three"));
    let fourth = bound_operation("tool-4", Some("model-four"));
    let mut fifth = bound_operation("tool-5", Some("model-five"));
    fifth.resolution = AttributionOperationResolution::Unresolved;
    let mut evidence =
        loaded_operation_evidence(vec![first, repeated, second, third, fourth, fifth]);
    evidence.operations_incomplete = true;
    assert_eq!(attribution_model(None, Some(&evidence)), None);
    assert_eq!(
        attribution_agent_label(None, Some(&evidence)).as_deref(),
        Some(concat!(
            "agent-assisted (mixed attribution; incomplete attribution; unresolved claims): ",
            "codex (router/model-one); codex (router/model-two); codex (router/model-three); ",
            "+2 more contributors"
        ))
    );
    assert_eq!(evidence.operations.len(), 6);
}
