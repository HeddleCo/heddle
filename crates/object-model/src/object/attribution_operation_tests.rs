// SPDX-License-Identifier: Apache-2.0
use super::*;

fn operation(tool: &str, path: &str) -> AttributionOperation {
    let mut identity = evidence().operation_identity();
    identity.scope.tool_call_id = Some(tool.into());
    AttributionOperation {
        identity,
        changes: vec![AttributionFileChange {
            path: path.into(),
            before: Some(ContentHash::compute_typed("blob", b"before")),
            after: Some(ContentHash::compute_typed("blob", b"after")),
        }],
        resolution: AttributionOperationResolution::ContentBound,
    }
}

#[test]
fn contributor_free_records_retain_original_canonical_bytes_and_hashes() {
    // The original seven-field HAE1 schema, independent of the extended type.
    #[derive(serde::Serialize)]
    struct OriginalEvidence<'a> {
        format_version: u8,
        harness: &'a Option<AttributionClaim>,
        harness_version: &'a Option<AttributionClaim>,
        harness_version_scope: Option<HarnessVersionScope>,
        selected: &'a ModelAttribution,
        response: &'a ModelAttribution,
        scope: &'a AttributionScope,
    }
    let value = evidence();
    let old = OriginalEvidence {
        format_version: value.format_version,
        harness: &value.harness,
        harness_version: &value.harness_version,
        harness_version_scope: value.harness_version_scope,
        selected: &value.selected,
        response: &value.response,
        scope: &value.scope,
    };
    let mut bytes = ATTRIBUTION_EVIDENCE_MAGIC.to_vec();
    bytes.extend(rmp_serde::to_vec_named(&old).expect("original encoding"));
    assert_eq!(value.to_bytes().expect("current encoding"), bytes);
    assert_eq!(
        value.to_blob().expect("current blob").hash(),
        Blob::new(bytes.clone()).hash()
    );
    let decoded = AttributionEvidenceV1::from_bytes(&bytes).expect("legacy decode");
    assert!(decoded.operations.is_empty());
    assert!(!decoded.operations_incomplete);
    assert_eq!(decoded, value);
}

#[test]
fn distinct_contributors_roundtrip_without_flattening_into_capture_agent() {
    let mut first = operation("tool-1", "src/first.rs");
    first.identity.scope.actor_id = Some("worker-1".into());
    let mut second = operation("tool-2", "src/second.rs");
    second.identity.scope.actor_id = Some("worker-2".into());
    second.identity.selected.model = Some(claim("different-model"));
    second.resolution = AttributionOperationResolution::Unresolved;
    second.changes.clear();
    let mut value = evidence();
    let original_agent = value.legacy_agent().expect("capture identity");
    value.operations = vec![first, second];
    value.operations_incomplete = true;
    let blob = value.to_blob().expect("mixed evidence");
    let decoded = AttributionEvidenceV1::from_blob(&blob).expect("decode");
    assert_eq!(decoded, value);
    assert!(decoded.legacy_agent().is_none());
    decoded
        .validate_legacy_agent(None)
        .expect("no flattened agent");
    assert!(
        decoded
            .validate_legacy_agent(Some(&original_agent))
            .is_err()
    );
    assert_eq!(
        decoded.operations[0].identity.scope.tool_call_id.as_deref(),
        Some("tool-1")
    );
    assert_eq!(
        decoded.operations[1].resolution,
        AttributionOperationResolution::Unresolved
    );
    assert_eq!(value.operation_identity(), evidence().operation_identity());
    let mut changed = value.clone();
    changed.operations[0].changes[0].after = Some(ContentHash::compute_typed("blob", b"changed"));
    assert_ne!(
        blob.hash(),
        changed.to_blob().expect("changed transition").hash()
    );
    changed = value;
    changed.operations_incomplete = false;
    assert_ne!(
        blob.hash(),
        changed.to_blob().expect("changed coverage").hash()
    );
}

#[test]
fn incomplete_coverage_cannot_project_capture_model_as_a_producer() {
    let mut value = evidence();
    let agent = value.legacy_agent().expect("capture model");
    value.operations_incomplete = true;
    assert!(value.operations.is_empty());
    assert!(value.legacy_agent().is_none());
    value
        .validate_legacy_agent(None)
        .expect("unknown contributors");
    assert!(value.validate_legacy_agent(Some(&agent)).is_err());
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().expect("incomplete"))
        .expect("decode incomplete");
    assert_eq!(decoded, value);
}

#[test]
fn operation_only_identity_is_valid_but_unidentified_contributors_are_not() {
    let mut value = AttributionEvidenceV1 {
        operations: vec![operation("tool-1", "file.txt")],
        ..Default::default()
    };
    value
        .to_blob()
        .expect("independently identified contributor");
    value.operations[0].identity = AttributionOperationIdentity::default();
    assert!(value.to_blob().is_err());
    value.operations.clear();
    value.operations_incomplete = true;
    assert!(value.to_blob().is_err());
}

#[test]
fn content_binding_requires_tool_identity_and_real_file_transitions() {
    let valid = operation("tool-1", "file.txt");
    valid.validate().expect("bound edit");
    let mut bad = valid.clone();
    bad.identity.scope.tool_call_id = None;
    assert!(bad.validate().is_err());
    bad.resolution = AttributionOperationResolution::Unresolved;
    bad.validate().expect("explicit unresolved observation");
    bad = valid.clone();
    bad.changes.clear();
    assert!(bad.validate().is_err());
    bad = valid.clone();
    bad.changes[0].after = bad.changes[0].before;
    assert!(bad.validate().is_err());
    bad.changes[0].before = None;
    bad.changes[0].after = None;
    assert!(bad.validate().is_err());
    let mut created = valid.clone();
    created.changes[0].before = None;
    created.validate().expect("created file");
    let mut deleted = valid;
    deleted.changes[0].after = None;
    deleted.validate().expect("deleted file");
}

#[test]
fn operation_paths_are_bounded_normalized_and_unique() {
    for path in [
        "",
        "/absolute",
        "../outside",
        "src/../outside",
        "./file",
        "src//file",
        "src/",
        "C:/file",
        "src\\file",
        "line\nfile",
        "file\0name",
    ] {
        assert!(
            operation("tool-1", path).validate().is_err(),
            "accepted {path:?}"
        );
    }
    operation("tool-1", "docs/日本語 notes.md")
        .validate()
        .expect("relative Unicode path");
    operation("tool-1", &"x".repeat(ATTRIBUTION_PATH_MAX_BYTES))
        .validate()
        .expect("maximum path");
    assert!(
        operation("tool-1", &"x".repeat(ATTRIBUTION_PATH_MAX_BYTES + 1))
            .validate()
            .is_err()
    );
    let mut duplicate = operation("tool-1", "file.txt");
    duplicate.changes.push(duplicate.changes[0].clone());
    assert!(duplicate.validate().is_err());
    duplicate.changes = (0..=ATTRIBUTION_MAX_FILE_CHANGES)
        .map(|index| AttributionFileChange {
            path: format!("file-{index}"),
            before: None,
            after: Some(ContentHash::compute_typed("blob", b"new")),
        })
        .collect();
    assert!(duplicate.validate().is_err());
}

#[test]
fn contributor_records_enforce_count_total_size_and_identity_validation() {
    let mut value = evidence();
    value.operations = (0..=ATTRIBUTION_MAX_OPERATIONS)
        .map(|index| operation(&format!("tool-{index}"), "file.txt"))
        .collect();
    assert!(value.to_blob().is_err());
    value.operations.pop();
    value.to_blob().expect("maximum operation count");
    value.operations[0].identity.response.model = Some(claim("misclassified-response"));
    assert!(value.to_blob().is_err());
    value.operations[0].identity.response.model = None;
    for operation in &mut value.operations {
        operation.changes = (0..ATTRIBUTION_MAX_FILE_CHANGES)
            .map(|index| AttributionFileChange {
                path: format!("{index:02}{}", "x".repeat(ATTRIBUTION_PATH_MAX_BYTES - 2)),
                before: None,
                after: Some(ContentHash::compute_typed("blob", b"new")),
            })
            .collect();
    }
    assert!(matches!(
        value.to_blob(),
        Err(AttributionEvidenceError::TooLarge)
    ));
}

#[test]
fn contributor_schema_rejects_recursive_or_arbitrary_payloads() {
    let mut value = evidence();
    value.operations.push(operation("tool-1", "file.txt"));
    let mut json = serde_json::to_value(value).expect("structured evidence");
    json["operations"][0]["identity"]["operations"] = serde_json::json!([]);
    assert!(AttributionEvidenceV1::from_bytes(&encoded_json(&json)).is_err());
    json["operations"][0]["identity"]
        .as_object_mut()
        .expect("identity")
        .remove("operations");
    json["operations"][0]["changes"][0]["contents"] = serde_json::json!("raw source");
    assert!(AttributionEvidenceV1::from_bytes(&encoded_json(&json)).is_err());
}

#[test]
fn collection_methods_are_separate_from_claim_provenance_and_retain_conflicts() {
    let mut value = evidence();
    let mut hook = operation("tool-1", "file.txt");
    hook.identity.collection_methods = vec![AttributionCollectionMethod::Hook];
    let mut transcript = hook.clone();
    transcript.identity.collection_methods = vec![AttributionCollectionMethod::Transcript];
    transcript.identity.selected.model = Some(AttributionClaim::new(
        "conflicting-model",
        AttributionBasis::RequestReported,
        AttributionSource::Transcript,
    ));
    transcript.resolution = AttributionOperationResolution::Unresolved;
    value.operations = vec![hook, transcript];
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().expect("conflicting claims"))
        .expect("decode");
    assert_eq!(decoded, value);
    for method in [
        AttributionCollectionMethod::Hook,
        AttributionCollectionMethod::EventStream,
        AttributionCollectionMethod::OpenTelemetry,
        AttributionCollectionMethod::Transcript,
        AttributionCollectionMethod::Proxy,
        AttributionCollectionMethod::Explicit,
    ] {
        value.operations[0].identity.collection_methods = vec![method];
        let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().expect("method"))
            .expect("decode method");
        assert_eq!(
            decoded.operations[0].identity.collection_methods,
            vec![method]
        );
        assert_eq!(
            decoded.operations[0]
                .identity
                .selected
                .model
                .as_ref()
                .expect("model")
                .source,
            AttributionSource::HarnessHook
        );
    }
    assert!(
        evidence()
            .operation_identity()
            .collection_methods
            .is_empty()
    );
}

#[test]
fn coalesced_collector_origins_are_bounded_and_unique() {
    let mut value = operation("tool-1", "file.txt");
    value.identity.collection_methods = vec![
        AttributionCollectionMethod::Hook,
        AttributionCollectionMethod::EventStream,
        AttributionCollectionMethod::OpenTelemetry,
        AttributionCollectionMethod::Transcript,
        AttributionCollectionMethod::Proxy,
        AttributionCollectionMethod::Explicit,
    ];
    value.validate().expect("all distinct collector origins");
    let mut evidence = evidence();
    evidence.operations.push(value.clone());
    assert_eq!(
        AttributionEvidenceV1::from_blob(&evidence.to_blob().expect("coalesced origins"))
            .expect("decode origins"),
        evidence
    );
    value
        .identity
        .collection_methods
        .push(AttributionCollectionMethod::Hook);
    assert!(value.validate().is_err());
    value.identity.collection_methods = vec![AttributionCollectionMethod::Hook; 2];
    assert!(value.validate().is_err());
}
