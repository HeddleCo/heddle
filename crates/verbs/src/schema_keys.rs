// SPDX-License-Identifier: Apache-2.0
//! Pure JSON Schema property-key walks over `serde_json::Value`.
//!
//! Resolves `$ref`s and unions to collect the declared property keys of a
//! schema. Used by the wire-envelope tests to assert a rendered payload's
//! keys are all declared by its registered schema.

use std::collections::BTreeSet;

use serde_json::Value;

/// All top-level property keys a schema declares, following `$ref`s and
/// `anyOf`/`oneOf`/`allOf` unions.
pub fn schema_property_keys(schema: &Value) -> BTreeSet<String> {
    schema_property_keys_from(schema, schema)
}

fn schema_property_keys_from(root: &Value, schema: &Value) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();

    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = schema.get(combinator).and_then(|value| value.as_array()) {
            for variant in variants {
                keys.extend(schema_property_keys_from(root, variant));
            }
        }
    }

    if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str())
        && let Some(target) = schema_ref_target(root, reference)
    {
        keys.extend(schema_property_keys_from(root, target));
    }

    keys
}

/// Resolve a JSON Schema `$ref` against `root` (fragment form `#/…` only).
pub fn schema_ref_target<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for part in path.split('/') {
        let decoded = part.replace("~1", "/").replace("~0", "~");
        current = current.get(&decoded)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_property_keys_empty_when_no_properties() {
        let schema = json!({ "type": "object" });
        assert!(schema_property_keys(&schema).is_empty());
    }

    #[test]
    fn schema_property_keys_follow_ref_and_any_of() {
        let schema = json!({
            "$defs": {
                "Inner": { "properties": { "b": {}, "c": {} } }
            },
            "anyOf": [
                { "properties": { "a": {} } },
                { "$ref": "#/$defs/Inner" }
            ]
        });
        let keys = schema_property_keys(&schema);
        assert!(keys.contains("a"));
        assert!(keys.contains("b"));
        assert!(keys.contains("c"));
    }

    #[test]
    fn schema_ref_target_decodes_json_pointer_escapes() {
        let root = json!({ "a/b": { "x": 1 }, "c~d": { "y": 2 } });
        assert_eq!(
            schema_ref_target(&root, "#/a~1b").and_then(|v| v.get("x")),
            Some(&json!(1))
        );
        assert_eq!(
            schema_ref_target(&root, "#/c~0d").and_then(|v| v.get("y")),
            Some(&json!(2))
        );
    }
}
