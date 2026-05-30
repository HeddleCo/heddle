// SPDX-License-Identifier: Apache-2.0
//! Measurement harness for the heddle#327 spike. These tests ARE the
//! "measure output quality" deliverable: they prove both emitters cover the
//! documented contract, prove the attribute-design claims (doc comments →
//! descriptions, example carry-through), and print the comparison table the
//! spike doc cites. Run with `--nocapture` to see the table.

use std::collections::BTreeSet;

use heddle_cli_macro_poc::{
    custom_path, documented_sample_keys, init_example, property_keys, schemars_path,
};
use serde_json::Value;

/// The drift contract: every key the documented sample asserts must appear as a
/// property in BOTH emitters' schemas. This is the property `heddle doctor
/// schemas` enforces — the macro is only viable if whichever emitter it picks
/// satisfies it.
#[test]
fn both_emitters_cover_the_documented_sample_keys() {
    let documented: BTreeSet<&str> = documented_sample_keys().into_iter().collect();

    let schemars_keys: BTreeSet<String> = property_keys(&schemars_path::schema())
        .into_iter()
        .collect();
    let custom_keys: BTreeSet<String> = property_keys(&custom_path::schema()).into_iter().collect();

    for key in &documented {
        assert!(
            schemars_keys.contains(*key),
            "schemars schema is missing documented key `{key}`"
        );
        assert!(
            custom_keys.contains(*key),
            "custom-emitter schema is missing documented key `{key}`"
        );
    }
}

/// Spike question #1, part A: narrative descriptions can live in `#[doc]`.
/// schemars lifts `///` doc comments into each property's `description` with no
/// extra attribute. This is the empirical proof behind the spec's claim.
#[test]
fn schemars_emits_field_descriptions_from_doc_comments() {
    let schema = schemars_path::schema();
    let props = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("schemars schema has a properties object");

    let desc = props
        .get("path")
        .and_then(|p| p.get("description"))
        .and_then(Value::as_str)
        .expect("`path` field carries a description lifted from its doc comment");

    assert!(
        desc.contains(".heddle"),
        "description should be the doc-comment text, got: {desc:?}"
    );
}

/// Spike question #1, part B: examples. schemars carries the
/// `#[schemars(example = \"init_example\")]` payload into the schema. This is
/// the awkward part the spec calls out — the example must be a free function,
/// not inline data — but it DOES land in the output.
#[test]
fn schemars_carries_the_example_payload() {
    let schema = schemars_path::schema();
    // schemars 0.8 places examples under the root metadata as `examples`.
    let has_example = schema
        .get("examples")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    assert!(
        has_example,
        "schemars schema should carry the example: {schema}"
    );
}

/// The custom emitter, by construction, embeds the same documented example.
#[test]
fn custom_emitter_embeds_the_documented_example() {
    let schema = custom_path::schema();
    let example = &schema["examples"][0];
    assert_eq!(example["output_kind"], "init");
    assert_eq!(example["next_action"], "heddle adopt --ref main");
    // The example is exactly what serializing the canonical value produces.
    assert_eq!(*example, serde_json::to_value(init_example()).unwrap());
}

/// Print the comparison table the spike doc cites. Not an assertion — a probe.
#[test]
fn print_measurement_table() {
    let s = schemars_path::schema();
    let c = custom_path::schema();

    let s_pretty = serde_json::to_string_pretty(&s).unwrap();
    let c_pretty = serde_json::to_string_pretty(&c).unwrap();

    let s_keys = property_keys(&s).len();
    let c_keys = property_keys(&c).len();

    let s_has_defs = s.get("definitions").is_some() || s.get("$defs").is_some();
    let s_const = s
        .pointer("/properties/output_kind/const")
        .or_else(|| s.pointer("/properties/output_kind/enum"))
        .is_some();
    let c_const = c.pointer("/properties/output_kind/const").is_some();

    println!("\n=== heddle#327 PoC measurement — `init` verb, both ways ===");
    println!("{:<34} | {:>10} | {:>10}", "metric", "schemars", "custom");
    println!("{:-<34}-+-{:-<10}-+-{:-<10}", "", "", "");
    println!(
        "{:<34} | {:>10} | {:>10}",
        "pretty-printed schema bytes",
        s_pretty.len(),
        c_pretty.len()
    );
    println!(
        "{:<34} | {:>10} | {:>10}",
        "top-level property keys", s_keys, c_keys
    );
    println!(
        "{:<34} | {:>10} | {:>10}",
        "uses $ref/definitions (nested)", s_has_defs, false
    );
    println!(
        "{:<34} | {:>10} | {:>10}",
        "output_kind pinned (const/enum)", s_const, c_const
    );
    println!("{:<34} | {:>10} | {:>10}", "carries example", true, true);
    println!(
        "{:<34} | {:>10} | {:>10}",
        "needs schemars dep", true, false
    );
    println!("===========================================================\n");

    println!("--- schemars schema ---\n{s_pretty}\n");
    println!("--- custom-emitter schema ---\n{c_pretty}\n");
}
