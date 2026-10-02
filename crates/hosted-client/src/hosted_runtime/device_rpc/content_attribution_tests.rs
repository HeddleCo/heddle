use super::*;
use objects::store::{InMemoryStore, ObjectStore};

fn claim(
    value: &str,
    basis: native::AttributionBasis,
    source: native::AttributionSource,
) -> native::AttributionClaim {
    native::AttributionClaim::new(value, basis, source)
}

fn fixture() -> (InMemoryStore, native::State, native::AttributionEvidenceV1) {
    let store = InMemoryStore::new();
    let evidence = native::AttributionEvidenceV1 {
        harness: Some(claim(
            "codex",
            native::AttributionBasis::Observed,
            native::AttributionSource::Process,
        )),
        harness_version: Some(claim(
            "1.2.3",
            native::AttributionBasis::Observed,
            native::AttributionSource::SessionMetadata,
        )),
        harness_version_scope: Some(native::HarnessVersionScope::SessionCreation),
        scope: native::AttributionScope {
            actor_id: Some("actor-child".into()),
            parent_actor_id: Some("actor-parent".into()),
            harness_session_id: Some("session-child".into()),
            parent_harness_session_id: Some("session-parent".into()),
            turn_id: Some("turn-child".into()),
            message_id: Some("message-child".into()),
            root_turn_id: Some("turn-parent".into()),
            tool_call_id: Some("tool-child".into()),
            request_id: Some("request-child".into()),
            response_id: Some("response-child".into()),
            attempt_id: Some("attempt-child".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let blob = evidence.to_blob().expect("blob");
    store.put_blob(&blob).expect("put");
    let state = native::State::new(
        native::ContentHash::compute(b"tree"),
        vec![],
        native::Attribution::human(native::Principal::new("Ada", "ada@example.test")),
    )
    .with_attribution_evidence(blob.hash());
    (store, state, evidence)
}

#[test]
fn state_summary_carries_bound_harness_and_unknown_model_without_timeline() {
    let (store, state, evidence) = fixture();
    let summary = super::super::content_summary::summary(&store, &state, None).expect("summary");
    assert!(summary.agent.is_none());
    let bound = summary.attribution.expect("authoritative evidence");
    assert_eq!(
        bound.evidence_hash,
        evidence.to_blob().expect("blob").hash().as_bytes()
    );
    let projected = bound.evidence.expect("claims");
    assert_eq!(projected.harness.expect("harness").value, "codex");
    assert!(projected.selected.expect("selected").model.is_none());
    assert!(projected.response.expect("response").model.is_none());
    let scope = projected.scope.expect("scope");
    assert_eq!(scope.actor_id.as_deref(), Some("actor-child"));
    assert_eq!(scope.parent_actor_id.as_deref(), Some("actor-parent"));
    assert_eq!(scope.request_id.as_deref(), Some("request-child"));
    assert_eq!(scope.message_id.as_deref(), Some("message-child"));
    assert_eq!(scope.root_turn_id.as_deref(), Some("turn-parent"));
    assert_eq!(scope.tool_call_id.as_deref(), Some("tool-child"));
    assert_eq!(scope.response_id.as_deref(), Some("response-child"));
}

#[test]
fn projection_preserves_selected_and_response_facts_and_provenance() {
    let (store, mut state, mut evidence) = fixture();
    evidence.selected.provider = Some(claim(
        "router",
        native::AttributionBasis::Configured,
        native::AttributionSource::Configuration,
    ));
    evidence.selected.model = Some(claim(
        "auto",
        native::AttributionBasis::RequestReported,
        native::AttributionSource::Request,
    ));
    evidence.response.model = Some(claim(
        "actual",
        native::AttributionBasis::ResponseReported,
        native::AttributionSource::Response,
    ));
    evidence
        .response
        .model
        .as_mut()
        .expect("model")
        .observation_id = Some("response-child".into());
    let blob = evidence.to_blob().expect("blob");
    store.put_blob(&blob).expect("put");
    state.attribution_evidence = Some(blob.hash());
    state.attribution.agent = evidence.legacy_agent();
    let bound = attribution(&store, &state)
        .expect("projection")
        .expect("bound");
    let projected = bound.evidence.expect("evidence");
    let selected = projected.selected.expect("selected");
    assert_eq!(selected.provider.expect("provider").value, "router");
    assert_eq!(selected.model.expect("selected model").value, "auto");
    let response = projected.response.expect("response");
    assert!(response.provider.is_none());
    let model = response.model.expect("response model");
    assert_eq!(model.value, "actual");
    assert_eq!(
        model.basis,
        shared::AttributionBasis::ResponseReported as i32
    );
    assert_eq!(model.source, shared::AttributionSource::Response as i32);
    assert_eq!(model.observation_id.as_deref(), Some("response-child"));
}

#[test]
fn required_missing_or_corrupt_evidence_fails_closed() {
    let (store, mut state, _) = fixture();
    state.attribution_evidence = Some(native::ContentHash::compute(b"missing"));
    assert!(super::super::content_summary::summary(&store, &state, None).is_err());
    let corrupt = native::Blob::new(b"bad".to_vec());
    store.put_blob(&corrupt).expect("put");
    state.attribution_evidence = Some(corrupt.hash());
    assert!(super::super::content_summary::summary(&store, &state, None).is_err());
}

#[test]
fn legacy_summary_never_manufactures_new_evidence() {
    let (store, mut state, _) = fixture();
    state.attribution_evidence = None;
    let summary = super::super::content_summary::summary(&store, &state, None).expect("legacy");
    assert!(summary.attribution.is_none());
    assert!(summary.agent.is_none());
}

#[test]
fn projection_preserves_independent_contributors_and_content_transition_presence() {
    let (store, mut state, mut evidence) = fixture();
    evidence.selected.model = Some(claim(
        "capture-selected",
        native::AttributionBasis::Configured,
        native::AttributionSource::Configuration,
    ));
    let mut identity = native::AttributionOperationIdentity {
        harness: Some(claim(
            "worker-harness",
            native::AttributionBasis::Observed,
            native::AttributionSource::HarnessHook,
        )),
        harness_version: Some(claim(
            "2.3.4",
            native::AttributionBasis::Observed,
            native::AttributionSource::HarnessHook,
        )),
        harness_version_scope: Some(native::HarnessVersionScope::CurrentInvocation),
        collection_methods: vec![
            native::AttributionCollectionMethod::Hook,
            native::AttributionCollectionMethod::EventStream,
        ],
        scope: native::AttributionScope {
            heddle_session_id: Some("heddle-session".into()),
            heddle_segment_id: Some("heddle-segment".into()),
            harness_instance_id: Some("worker-instance".into()),
            harness_session_id: Some("worker-session".into()),
            actor_id: Some("worker-actor".into()),
            parent_actor_id: Some("actor-child".into()),
            parent_harness_session_id: Some("session-child".into()),
            turn_id: Some("worker-turn".into()),
            message_id: Some("worker-message".into()),
            root_turn_id: Some("turn-parent".into()),
            tool_call_id: Some("worker-tool".into()),
            request_id: Some("worker-request".into()),
            response_id: Some("worker-response".into()),
            attempt_id: Some("worker-attempt".into()),
        },
        ..Default::default()
    };
    identity.selected = native::ModelAttribution {
        provider: Some(claim(
            "worker-router",
            native::AttributionBasis::RequestReported,
            native::AttributionSource::Request,
        )),
        model: Some(claim(
            "worker-selected",
            native::AttributionBasis::RequestReported,
            native::AttributionSource::Request,
        )),
        version: Some(claim(
            "selected-revision",
            native::AttributionBasis::RequestReported,
            native::AttributionSource::Request,
        )),
        thought_level: Some(claim(
            "high",
            native::AttributionBasis::RequestReported,
            native::AttributionSource::Request,
        )),
    };
    identity.response = native::ModelAttribution {
        provider: None,
        model: Some(claim(
            "worker-reported",
            native::AttributionBasis::ResponseReported,
            native::AttributionSource::Response,
        )),
        version: Some(claim(
            "response-revision",
            native::AttributionBasis::ResponseReported,
            native::AttributionSource::Response,
        )),
        thought_level: Some(claim(
            "low",
            native::AttributionBasis::ResponseReported,
            native::AttributionSource::Response,
        )),
    };
    identity
        .response
        .model
        .as_mut()
        .expect("model")
        .observation_id = Some("worker-response".into());
    let before = native::Blob::new(b"before".to_vec()).hash();
    let after = native::Blob::new(b"after".to_vec()).hash();
    evidence.operations = vec![
        native::AttributionOperation {
            identity,
            changes: vec![
                native::AttributionFileChange {
                    path: "src/modified.rs".into(),
                    before: Some(before),
                    after: Some(after),
                },
                native::AttributionFileChange {
                    path: "src/created.rs".into(),
                    before: None,
                    after: Some(after),
                },
                native::AttributionFileChange {
                    path: "src/deleted.rs".into(),
                    before: Some(before),
                    after: None,
                },
            ],
            resolution: native::AttributionOperationResolution::ContentBound,
        },
        native::AttributionOperation {
            identity: native::AttributionOperationIdentity {
                harness: Some(claim(
                    "other-harness",
                    native::AttributionBasis::Observed,
                    native::AttributionSource::Transcript,
                )),
                collection_methods: vec![native::AttributionCollectionMethod::Transcript],
                scope: native::AttributionScope {
                    actor_id: Some("other-actor".into()),
                    attempt_id: Some("other-attempt".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            changes: vec![],
            resolution: native::AttributionOperationResolution::Unresolved,
        },
    ];
    evidence.operations_incomplete = true;
    let blob = evidence.to_blob().expect("valid contributors");
    store.put_blob(&blob).expect("put");
    state.attribution_evidence = Some(blob.hash());
    state.attribution.agent = evidence.legacy_agent();
    let projected = attribution(&store, &state)
        .expect("project")
        .expect("bound")
        .evidence
        .expect("evidence");
    assert_eq!(
        projected
            .selected
            .expect("caller")
            .model
            .expect("caller model")
            .value,
        "capture-selected"
    );
    assert_eq!(
        projected.scope.expect("caller scope").actor_id.as_deref(),
        Some("actor-child")
    );
    assert!(projected.operations_incomplete);
    assert_eq!(projected.operations.len(), 2);
    let bound = &projected.operations[0];
    assert_eq!(
        bound.resolution,
        shared::AttributionOperationResolution::ContentBound as i32
    );
    let identity = bound.identity.as_ref().expect("identity");
    assert_eq!(
        identity.harness.as_ref().expect("harness").value,
        "worker-harness"
    );
    assert_eq!(
        identity
            .harness_version
            .as_ref()
            .expect("harness version")
            .value,
        "2.3.4"
    );
    assert_eq!(
        identity.harness_version_scope,
        Some(shared::HarnessVersionScope::CurrentInvocation as i32)
    );
    assert_eq!(
        identity.collection_methods,
        vec![
            shared::AttributionCollectionMethod::Hook as i32,
            shared::AttributionCollectionMethod::EventStream as i32
        ]
    );
    assert_eq!(
        identity.scope,
        Some(shared::AttributionScope {
            heddle_session_id: Some("heddle-session".into()),
            heddle_segment_id: Some("heddle-segment".into()),
            harness_instance_id: Some("worker-instance".into()),
            harness_session_id: Some("worker-session".into()),
            actor_id: Some("worker-actor".into()),
            parent_actor_id: Some("actor-child".into()),
            parent_harness_session_id: Some("session-child".into()),
            turn_id: Some("worker-turn".into()),
            message_id: Some("worker-message".into()),
            root_turn_id: Some("turn-parent".into()),
            tool_call_id: Some("worker-tool".into()),
            request_id: Some("worker-request".into()),
            response_id: Some("worker-response".into()),
            attempt_id: Some("worker-attempt".into()),
        })
    );
    let selected = identity.selected.as_ref().expect("selected");
    assert_eq!(
        selected.provider.as_ref().expect("provider").value,
        "worker-router"
    );
    assert_eq!(
        selected.model.as_ref().expect("model").value,
        "worker-selected"
    );
    assert_eq!(
        selected.version.as_ref().expect("version").value,
        "selected-revision"
    );
    assert_eq!(
        selected
            .thought_level
            .as_ref()
            .expect("thought level")
            .value,
        "high"
    );
    let response = identity.response.as_ref().expect("response");
    assert!(response.provider.is_none());
    let model = response.model.as_ref().expect("model");
    assert_eq!(model.value, "worker-reported");
    assert_eq!(
        model.basis,
        shared::AttributionBasis::ResponseReported as i32
    );
    assert_eq!(model.source, shared::AttributionSource::Response as i32);
    assert_eq!(model.observation_id.as_deref(), Some("worker-response"));
    assert_eq!(
        response.version.as_ref().expect("version").value,
        "response-revision"
    );
    assert_eq!(
        response
            .thought_level
            .as_ref()
            .expect("thought level")
            .value,
        "low"
    );
    assert_eq!(bound.changes[0].path, "src/modified.rs");
    assert_eq!(
        bound.changes[0].before.as_ref().expect("before").value,
        before.as_bytes()
    );
    assert_eq!(
        bound.changes[0].after.as_ref().expect("after").value,
        after.as_bytes()
    );
    assert!(bound.changes[1].before.is_none());
    assert_eq!(
        bound.changes[1].after.as_ref().expect("created").value,
        after.as_bytes()
    );
    assert_eq!(
        bound.changes[2].before.as_ref().expect("deleted").value,
        before.as_bytes()
    );
    assert!(bound.changes[2].after.is_none());
    let unresolved = &projected.operations[1];
    assert_eq!(
        unresolved.resolution,
        shared::AttributionOperationResolution::Unresolved as i32
    );
    assert!(unresolved.changes.is_empty());
    let identity = unresolved.identity.as_ref().expect("other identity");
    assert_eq!(
        identity.harness.as_ref().expect("other harness").value,
        "other-harness"
    );
    assert!(identity.harness_version.is_none());
    assert!(identity.harness_version_scope.is_none());
    assert!(
        identity
            .selected
            .as_ref()
            .expect("selected")
            .model
            .is_none()
    );
    assert!(
        identity
            .response
            .as_ref()
            .expect("response")
            .model
            .is_none()
    );
    assert_eq!(
        identity.collection_methods,
        vec![shared::AttributionCollectionMethod::Transcript as i32]
    );
    let scope = identity.scope.as_ref().expect("other scope");
    assert_eq!(scope.actor_id.as_deref(), Some("other-actor"));
    assert_eq!(scope.attempt_id.as_deref(), Some("other-attempt"));
    assert!(scope.parent_actor_id.is_none());
    assert!(scope.tool_call_id.is_none());
    assert!(scope.request_id.is_none());
    assert!(scope.response_id.is_none());
}

#[test]
fn operation_projection_preserves_each_collection_method_without_guessing() {
    for (native, wire) in [
        (None, None),
        (
            Some(native::AttributionCollectionMethod::Hook),
            Some(shared::AttributionCollectionMethod::Hook),
        ),
        (
            Some(native::AttributionCollectionMethod::EventStream),
            Some(shared::AttributionCollectionMethod::EventStream),
        ),
        (
            Some(native::AttributionCollectionMethod::OpenTelemetry),
            Some(shared::AttributionCollectionMethod::OpenTelemetry),
        ),
        (
            Some(native::AttributionCollectionMethod::Transcript),
            Some(shared::AttributionCollectionMethod::Transcript),
        ),
        (
            Some(native::AttributionCollectionMethod::Proxy),
            Some(shared::AttributionCollectionMethod::Proxy),
        ),
        (
            Some(native::AttributionCollectionMethod::Explicit),
            Some(shared::AttributionCollectionMethod::Explicit),
        ),
    ] {
        let projected = operation(native::AttributionOperation {
            identity: native::AttributionOperationIdentity {
                collection_methods: native.into_iter().collect(),
                ..Default::default()
            },
            changes: vec![],
            resolution: native::AttributionOperationResolution::Unresolved,
        });
        assert_eq!(
            projected.identity.expect("identity").collection_methods,
            wire.into_iter()
                .map(|method| method as i32)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn legacy_projection_retains_empty_operations_and_no_incompleteness_claim() {
    let (_, _, evidence) = fixture();
    let projected = project(evidence);
    assert!(projected.operations.is_empty());
    assert!(!projected.operations_incomplete);
}
