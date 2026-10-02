// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::object::{Agent, Attribution, Blob, ContentHash, Principal};

fn claim(value: &str) -> AttributionClaim {
    AttributionClaim::new(
        value,
        AttributionBasis::RequestReported,
        AttributionSource::HarnessHook,
    )
}

fn evidence() -> AttributionEvidenceV1 {
    AttributionEvidenceV1 {
        harness: Some(claim("claude-code")),
        selected: ModelAttribution {
            provider: Some(claim("gateway-routing-key")),
            model: Some(claim("org/model:alias")),
            ..Default::default()
        },
        scope: AttributionScope {
            harness_session_id: Some("native-session".into()),
            actor_id: Some("child-2".into()),
            parent_actor_id: Some("main-1".into()),
            request_id: Some("request-4".into()),
            attempt_id: Some("attempt-2".into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn canonical_claims_roundtrip_and_bind_standard_blob_hash() {
    let mut value = evidence();
    value.response = ModelAttribution {
        provider: Some(AttributionClaim::new(
            "reported-backend",
            AttributionBasis::ResponseReported,
            AttributionSource::Response,
        )),
        model: Some(AttributionClaim::new(
            "org/model:exact-revision",
            AttributionBasis::ResponseReported,
            AttributionSource::Response,
        )),
        ..Default::default()
    };
    let blob = value.to_blob().unwrap();
    assert!(blob.content().starts_with(ATTRIBUTION_EVIDENCE_MAGIC));
    assert_eq!(
        blob.hash(),
        ContentHash::compute_typed("blob", blob.content())
    );
    assert_eq!(
        AttributionEvidenceV1::from_blob_with_hash(&blob, blob.hash()).unwrap(),
        value
    );
    assert_eq!(value.to_blob().unwrap(), blob);
    assert_eq!(
        value.selected.model.as_ref().unwrap().value,
        "org/model:alias"
    );
    assert_eq!(
        value.legacy_agent().unwrap().model,
        "org/model:exact-revision"
    );
    assert!(matches!(
        AttributionEvidenceV1::from_blob_with_hash(&blob, ContentHash::from_bytes([0; 32])),
        Err(AttributionEvidenceError::HashMismatch)
    ));
}

#[test]
fn unknown_model_preserves_harness_and_scoped_identity_without_legacy_agent() {
    let value = AttributionEvidenceV1 {
        selected: ModelAttribution::default(),
        ..evidence()
    };
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().unwrap()).unwrap();
    assert!(decoded.legacy_agent().is_none());
    assert_eq!(
        decoded.scope.harness_session_id.as_deref(),
        Some("native-session")
    );
    decoded
        .validate_attribution(&Attribution::human(Principal::new(
            "Author",
            "a@example.com",
        )))
        .unwrap();
    assert!(
        decoded
            .validate_legacy_agent(Some(&Agent::new("invented", "invented")))
            .is_err()
    );
}

#[test]
fn partial_response_never_combines_with_selection_or_overstates_execution() {
    let mut value = evidence();
    value.response.model = Some(AttributionClaim::new(
        "reported-model",
        AttributionBasis::ResponseReported,
        AttributionSource::Response,
    ));
    let projected = value.legacy_agent().unwrap();
    assert_eq!(projected.provider, "gateway-routing-key");
    assert_eq!(projected.model, "org/model:alias");
    value.validate_legacy_agent(Some(&projected)).unwrap();
    let mut wrong = projected.clone();
    wrong.model = "reported-model".into();
    assert!(value.validate_legacy_agent(Some(&wrong)).is_err());
    let mut policy = projected;
    policy.policy_id = Some("independent-policy".into());
    value.validate_legacy_agent(Some(&policy)).unwrap();
}

#[test]
fn versions_and_namespaces_remain_explicit_and_independent() {
    let mut value = evidence();
    value.harness_version = Some(claim("2.4.1+build.9"));
    assert!(value.to_blob().is_err());
    value.harness_version_scope = Some(HarnessVersionScope::SessionCreation);
    value.scope.heddle_session_id = Some("heddle-session".into());
    value.scope.heddle_segment_id = Some("heddle-segment".into());
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().unwrap()).unwrap();
    assert!(decoded.selected.version.is_none());
    assert_eq!(
        decoded.harness_version_scope,
        Some(HarnessVersionScope::SessionCreation)
    );
    let agent = decoded.legacy_agent().unwrap();
    assert_eq!(agent.session_id.as_deref(), Some("heddle-session"));
    assert_eq!(agent.parent.as_deref(), Some("main-1"));
    value.selected.version = Some(claim("explicit-revision"));
    value.selected.model = None;
    assert!(value.to_blob().is_err());
}

#[test]
fn empty_placeholder_and_misclassified_claims_are_rejected() {
    assert!(matches!(
        AttributionEvidenceV1::default().validate(),
        Err(AttributionEvidenceError::Empty)
    ));
    let mut value = evidence();
    value.harness.as_mut().unwrap().value = "unknown".into();
    assert!(value.validate().is_err());
    let mut value = evidence();
    value.response.model = Some(claim("model"));
    assert!(value.validate().is_err());
    let mut value = evidence();
    value.scope.parent_actor_id = value.scope.actor_id.clone();
    assert!(value.validate().is_err());
    let mut value = evidence();
    value.scope.heddle_segment_id = Some("segment-without-session".into());
    assert!(value.validate().is_err());
}

#[test]
fn oversized_and_nonidentity_content_cannot_enter_canonical_record() {
    for bad in [
        "PROMPT COMMAND /secret/path API_KEY=secret",
        "API_KEY=secret",
        "https://example.com/key",
        "/secret/path",
        "../secret",
        "org/../secret",
        "line\nmodel",
        "model\0key",
        "sk-secret",
        "ghp_token",
        " model",
        "",
    ] {
        let mut value = evidence();
        value.selected.model.as_mut().unwrap().value = bad.into();
        assert!(value.to_blob().is_err(), "accepted nonidentity content");
    }
    let mut value = evidence();
    value.scope.request_id = Some("x".repeat(ATTRIBUTION_VALUE_MAX_BYTES + 1));
    assert!(value.to_blob().is_err());
    assert!(matches!(
        AttributionEvidenceV1::from_bytes(&vec![0; ATTRIBUTION_EVIDENCE_MAX_BYTES + 1]),
        Err(AttributionEvidenceError::TooLarge)
    ));
}

#[test]
fn oversized_blob_is_rejected_before_hash_verification() {
    let blob = Blob::new(vec![0; ATTRIBUTION_EVIDENCE_MAX_BYTES + 1]);
    assert!(matches!(
        AttributionEvidenceV1::from_blob_with_hash(&blob, ContentHash::from_bytes([0; 32])),
        Err(AttributionEvidenceError::TooLarge)
    ));
}

fn encoded_json(value: &serde_json::Value) -> Vec<u8> {
    let mut bytes = ATTRIBUTION_EVIDENCE_MAGIC.to_vec();
    bytes.extend(rmp_serde::to_vec_named(value).unwrap());
    bytes
}

#[test]
fn unknown_fields_and_noncanonical_encoding_are_rejected() {
    let value = evidence();
    let mut top = serde_json::to_value(&value).unwrap();
    top["raw_payload"] = serde_json::json!("secret");
    assert!(AttributionEvidenceV1::from_bytes(&encoded_json(&top)).is_err());
    let mut nested = serde_json::to_value(&value).unwrap();
    nested["selected"]["model"]["prompt"] = serde_json::json!("secret");
    assert!(AttributionEvidenceV1::from_bytes(&encoded_json(&nested)).is_err());
    let unordered = encoded_json(&serde_json::to_value(&value).unwrap());
    assert!(matches!(
        AttributionEvidenceV1::from_bytes(&unordered),
        Err(AttributionEvidenceError::NonCanonical)
    ));
    let mut trailing = value.to_bytes().unwrap();
    trailing.push(0);
    assert!(AttributionEvidenceV1::from_bytes(&trailing).is_err());
    let mut wrong_version = value.clone();
    wrong_version.format_version = 2;
    assert!(matches!(
        wrong_version.to_blob(),
        Err(AttributionEvidenceError::UnsupportedVersion)
    ));
    let mut wrong_magic = value.to_bytes().unwrap();
    wrong_magic[3] = b'2';
    assert!(matches!(
        AttributionEvidenceV1::from_blob(&Blob::new(wrong_magic)),
        Err(AttributionEvidenceError::UnsupportedVersion)
    ));
}

#[test]
fn duplicate_fields_are_rejected_before_canonicalization() {
    let value = evidence();
    let mut bytes = value.to_bytes().unwrap();
    assert_eq!(bytes[4], 0x87); // Seven fields in the canonical named map.
    bytes[4] = 0x88;
    bytes.extend(rmp_serde::to_vec("format_version").unwrap());
    bytes.extend(rmp_serde::to_vec(&1u8).unwrap());
    assert!(AttributionEvidenceV1::from_bytes(&bytes).is_err());
}

proptest::proptest! {
    #[test]
    fn bounded_model_identifiers_roundtrip(model in "[a-z][a-z0-9_-]{0,60}") {
        let mut value = evidence(); value.selected.model.as_mut().unwrap().value = format!("model-{model}");
        let blob = value.to_blob().unwrap();
        proptest::prop_assert_eq!(AttributionEvidenceV1::from_blob(&blob).unwrap(), value);
    }
}

#[test]
fn exact_model_modifier_is_preserved_without_inventing_revision() {
    let mut value = evidence();
    value.selected.model.as_mut().unwrap().value = "claude-opus-4-8[1m]".into();
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().unwrap()).unwrap();
    assert_eq!(
        decoded.selected.model.as_ref().unwrap().value,
        "claude-opus-4-8[1m]"
    );
    assert!(decoded.selected.version.is_none());
}

#[path = "attribution_scope_tests.rs"]
mod scope_tests;

#[path = "attribution_operation_tests.rs"]
mod operation_tests;
