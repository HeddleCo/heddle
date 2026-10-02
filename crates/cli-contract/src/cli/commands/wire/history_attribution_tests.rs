// SPDX-License-Identifier: Apache-2.0
use super::*;
use objects::object::{
    Attribution, AttributionBasis, AttributionClaim, AttributionEvidenceV1, AttributionSource,
    ContentHash, Principal, State,
};

fn fixture() -> (State, AttributionEvidenceV1) {
    let evidence = AttributionEvidenceV1 {
        harness: Some(AttributionClaim::new(
            "codex",
            AttributionBasis::Observed,
            AttributionSource::Process,
        )),
        ..Default::default()
    };
    let state = State::new(
        ContentHash::compute(b"tree"),
        vec![],
        Attribution::human(Principal::new("Ada", "ada@example.test")),
    )
    .with_attribution_evidence(evidence.to_blob().expect("blob").hash());
    (state, evidence)
}

#[test]
fn history_json_retains_partial_agent_evidence_and_exact_state() {
    let (state, evidence) = fixture();
    let entry = StateEntry::from_state(&state, Some(evidence.clone()));
    let json = serde_json::to_value(&entry).expect("json");
    assert_eq!(json["is_agent_authored"], true);
    assert_eq!(json["agent"], "codex (model unknown)");
    assert_eq!(
        json["attribution_evidence"],
        serde_json::to_value(&evidence).expect("evidence json")
    );
    assert_eq!(json["state_id"], state.id().short());
    assert!(json["attribution_evidence"]["response"]["model"].is_null());
    let capture = ExpandedCaptureOutput::from_state(state, Some(evidence));
    let expanded = serde_json::to_value(capture).expect("expanded");
    assert_eq!(
        expanded["attribution_evidence"],
        json["attribution_evidence"]
    );
    assert_eq!(expanded["agent"], json["agent"]);
}

#[test]
fn history_json_preserves_legacy_absence() {
    let (mut state, _) = fixture();
    state.attribution_evidence = None;
    let json = serde_json::to_value(StateEntry::from_state(&state, None)).expect("json");
    assert_eq!(json["is_agent_authored"], false);
    assert!(json["agent"].is_null());
    assert!(json["attribution_evidence"].is_null());
}
