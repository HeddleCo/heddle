//! Asserts the exported JSON Schemas in `schemas/` stay faithful to the Rust
//! types and encode the load-bearing invariants — chiefly that they advertise
//! the same forward-compatibility posture the code has (`deny_unknown_fields`
//! OFF ⇒ `additionalProperties: true`) and that the pre-freeze DESIGN-§3.2/D18
//! members are present but NOT required.
//!
//! These schemas are hand-written (the crate is intentionally dependency-light;
//! see the final response notes on the schemars trade-off). This test is the
//! mechanism that keeps a hand-written schema honest.

use serde_json::Value;
use treadle_schema::fixture;

const BODY_SCHEMA: &str = include_str!("../schemas/ci_verdict_body.schema.json");
const SIGNED_SCHEMA: &str = include_str!("../schemas/signed_verdict.schema.json");

fn parse(name: &str, raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|e| panic!("{name} is not valid JSON: {e}"))
}

/// Walk every `"type": "object"` node and assert its `additionalProperties` does
/// not *close* the object (forward-compat requires open structs). Three legal
/// shapes:
///   - `true`  → open struct (the forward-compat default).
///   - a schema object (e.g. `{"type":"string"}`) → an open `map<…>` whose value
///     type is constrained; still open to new keys.
///   - `false` → ONLY allowed on the single-key `merged_with` BasisKind variant
///     wrapper, which is closed by construction (an externally-tagged struct
///     variant has exactly one key). The `branch` unit variant is now the bare
///     string `{"const":"branch"}` — not an object — so it never reaches this arm.
fn assert_forward_compat(node: &Value, path: &str) {
    if let Some(obj) = node.as_object() {
        if obj.get("type").and_then(Value::as_str) == Some("object") {
            match obj.get("additionalProperties") {
                Some(Value::Bool(true)) => {}
                // A map's value-type schema — open object, fine.
                Some(Value::Object(_)) => {}
                Some(Value::Bool(false)) => {
                    let is_variant_wrapper = obj
                        .get("required")
                        .and_then(Value::as_array)
                        .is_some_and(|r| r.len() == 1 && r[0].as_str() == Some("merged_with"));
                    assert!(
                        is_variant_wrapper,
                        "{path}: a struct object set additionalProperties:false but is not the \
                         BasisKind merged_with variant wrapper — forward-compat requires open \
                         structs"
                    );
                }
                other => panic!(
                    "{path}: object must declare additionalProperties (true / value-schema for \
                     forward-compat); got {other:?}"
                ),
            }
        }
        for (k, v) in obj {
            assert_forward_compat(v, &format!("{path}.{k}"));
        }
    } else if let Some(arr) = node.as_array() {
        for (i, v) in arr.iter().enumerate() {
            assert_forward_compat(v, &format!("{path}[{i}]"));
        }
    }
}

#[test]
fn schemas_are_valid_json() {
    let _ = parse("ci_verdict_body.schema.json", BODY_SCHEMA);
    let _ = parse("signed_verdict.schema.json", SIGNED_SCHEMA);
}

#[test]
fn schemas_advertise_forward_compat_everywhere() {
    let body = parse("body", BODY_SCHEMA);
    let signed = parse("signed", SIGNED_SCHEMA);
    assert_forward_compat(&body, "body");
    assert_forward_compat(&signed, "signed");
    // And explicitly at the two roots (the thing producers append to).
    assert_eq!(body["additionalProperties"], Value::Bool(true));
    assert_eq!(signed["additionalProperties"], Value::Bool(true));
}

#[test]
fn body_required_set_matches_the_non_optional_fields() {
    let body = parse("body", BODY_SCHEMA);
    let required: Vec<&str> = body["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // Exactly the fields with no skip_serializing_if in body.rs.
    let expected = [
        "schema_version",
        "repo",
        "state",
        "basis",
        "check",
        "outcome",
        "execution",
        "repro",
    ];
    for f in expected {
        assert!(required.contains(&f), "body.required must contain {f:?}");
    }
    // The optional members must NOT be required.
    for opt in ["log", "check_set_digest"] {
        assert!(
            !required.contains(&opt),
            "{opt:?} is optional (omit-when-absent) and must not be in body.required"
        );
        assert!(
            body["properties"].get(opt).is_some(),
            "{opt:?} must still be a declared property"
        );
    }
}

#[test]
fn prefreeze_d18_members_are_present_but_not_required() {
    let body = parse("body", BODY_SCHEMA);

    // body.check_set_digest
    assert!(body["properties"]["check_set_digest"].is_object());

    // check.node_id
    let check = &body["$defs"]["CheckDescriptor"];
    assert!(check["properties"]["node_id"].is_object());
    let check_required: Vec<&str> = check["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!check_required.contains(&"node_id"));

    // execution attestation block
    let exec = &body["$defs"]["Execution"];
    let exec_required: Vec<&str> = exec["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for f in [
        "runner_pool",
        "trust_tier",
        "isolation_tier",
        "materialization_proof",
        "secret_grants",
    ] {
        assert!(
            exec["properties"].get(f).is_some(),
            "Execution must declare pre-freeze member {f:?}"
        );
        assert!(
            !exec_required.contains(&f),
            "pre-freeze member {f:?} must not be required"
        );
    }

    // basis merged_with merge_algorithm_version + conflict_policy
    let merged = &body["$defs"]["BasisKind"]["oneOf"][1]["properties"]["merged_with"];
    for f in ["merge_algorithm_version", "conflict_policy"] {
        assert!(
            merged["properties"].get(f).is_some(),
            "merged_with must declare pre-freeze member {f:?}"
        );
    }
    let merged_required: Vec<&str> = merged["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(merged_required, vec!["target_state", "behind_count"]);
}

#[test]
fn signed_verdict_required_set_is_complete() {
    let signed = parse("signed", SIGNED_SCHEMA);
    let required: Vec<&str> = signed["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for f in [
        "body",
        "body_digest",
        "public_key",
        "signature",
        "signed_at",
        "signer_kind",
    ] {
        assert!(required.contains(&f), "signed.required must contain {f:?}");
    }
    assert_eq!(
        signed["properties"]["signer_kind"]["enum"],
        serde_json::json!(["service_account", "delegated", "device"]),
        "signed schema must expose every SignerKind token"
    );
}

/// The honesty harness: actually *validate serialized fixture instances against
/// the hand-written schema*. The structural-invariant tests above (required sets,
/// forward-compat) never instantiate the schema, so they could not catch a
/// shape mismatch like BasisKind::Branch serializing as `"branch"` while the
/// schema demanded `{"branch": null}`. This test compiles the schema and runs
/// every fixture (crucially `branch_basis_body`, the only `Branch` instance)
/// through it, so the schema can no longer drift from serde's real output.
#[test]
fn every_fixture_validates_against_the_body_schema() {
    let schema = parse("body", BODY_SCHEMA);
    let validator =
        jsonschema::validator_for(&schema).expect("ci_verdict_body.schema.json must compile");

    let fixtures: [(&str, Value); 5] = [
        (
            "passing_body",
            serde_json::to_value(fixture::passing_body()).unwrap(),
        ),
        (
            "failing_body",
            serde_json::to_value(fixture::failing_body()).unwrap(),
        ),
        (
            "maximal_body",
            serde_json::to_value(fixture::maximal_body()).unwrap(),
        ),
        (
            "merge_basis_body",
            serde_json::to_value(fixture::merge_basis_body()).unwrap(),
        ),
        (
            "branch_basis_body",
            serde_json::to_value(fixture::branch_basis_body()).unwrap(),
        ),
    ];

    for (name, instance) in &fixtures {
        let errors: Vec<String> = validator
            .iter_errors(instance)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "fixture {name:?} failed schema validation:\n  {}",
            errors.join("\n  ")
        );
    }
}

/// Pin the BasisKind shape explicitly from both directions: the canonical
/// `"branch"` string MUST validate, and the historically-wrong `{"branch": null}`
/// form MUST NOT. This is the regression guard for the schema/serde mismatch.
#[test]
fn basis_kind_branch_schema_accepts_string_rejects_object_form() {
    let schema = parse("body", BODY_SCHEMA);
    let basis_kind = &schema["$defs"]["BasisKind"];
    let validator = jsonschema::validator_for(basis_kind).expect("BasisKind subschema compiles");

    // serde's actual output for BasisKind::Branch — the bare string.
    assert!(
        validator.is_valid(&Value::String("branch".into())),
        "schema must accept the bare \"branch\" string (serde's real Branch form)"
    );
    // The old, wrong internally-tagged form must now be rejected.
    assert!(
        !validator.is_valid(&serde_json::json!({ "branch": null })),
        "schema must REJECT the {{\"branch\": null}} object form — it is not serde's output"
    );
    // The struct variant still validates.
    assert!(
        validator.is_valid(&serde_json::json!({
            "merged_with": { "target_state": "hd-x", "behind_count": 0 }
        })),
        "schema must accept the merged_with struct variant"
    );
}
