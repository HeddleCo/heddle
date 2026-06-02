// SPDX-License-Identifier: Apache-2.0
//! Measurement harness for the heddle#327 spike. These tests ARE the
//! "measure output quality" deliverable: they prove both emitters cover the
//! documented contract, prove the attribute-design claims (doc comments →
//! descriptions, example carry-through), and — crucially — ASSERT the two
//! schemars costs the decision doc cites: the unpinned `output_kind`
//! discriminator and the phantom `verification` property schemars re-introduces
//! from the real `#[serde(skip_serializing)]` `trust` field. The numbers in the
//! doc are guarded by these assertions, not merely printed. Run with
//! `--nocapture` to also see the comparison table.

use std::collections::BTreeSet;

use heddle_cli_macro_poc::{
    custom_path, documented_sample_keys, init_example, property_keys, schemars_path,
};
use serde_json::Value;

/// `output_kind` is "pinned" if the schema constrains it to EXACTLY ONE value —
/// a `const`, or a single-element `enum`. A multi-element `enum` (e.g.
/// `["init","status"]`) does NOT pin the discriminator: it still admits more
/// than one value, so it must not satisfy this predicate.
fn output_kind_pinned(schema: &Value) -> bool {
    if schema.pointer("/properties/output_kind/const").is_some() {
        return true;
    }
    schema
        .pointer("/properties/output_kind/enum")
        .and_then(Value::as_array)
        .is_some_and(|variants| variants.len() == 1)
}

fn serialized_example_keys() -> BTreeSet<String> {
    serde_json::to_value(init_example())
        .unwrap()
        .as_object()
        .expect("init_example serializes to an object")
        .keys()
        .cloned()
        .collect()
}

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

/// Spike answer Q2 — the measured discriminator gap, ASSERTED (not just
/// printed). schemars renders `output_kind` as a bare `{"type":"string"}` from
/// a plain `&'static str`; the custom emitter pins it to `"const":"init"`. The
/// decision doc treats this as the single concrete schemars cost and proposes a
/// #205 helper for it — so the gate must fail if a schemars upgrade starts
/// emitting a const/enum here (making the helper unnecessary) or the custom
/// emitter stops pinning it.
#[test]
fn discriminator_gap_is_real_schemars_unpinned_custom_pinned() {
    let schemars_pinned = output_kind_pinned(&schemars_path::schema());
    let custom_pinned = output_kind_pinned(&custom_path::schema());
    assert!(
        !schemars_pinned,
        "schemars unexpectedly pinned output_kind — the discriminator gap (and \
         the proposed #205 helper) is now stale; revisit the decision doc"
    );
    assert!(
        custom_pinned,
        "custom emitter stopped pinning output_kind as a const"
    );
}

/// Spike answer Q2 — the skip_serializing drift, ASSERTED. The real
/// `InitOutput.trust` field is `#[serde(skip_serializing)] #[serde(rename =
/// "verification")]`. serde drops it from the wire bytes; schemars' derive
/// re-introduces a `verification` property (as `writeOnly`) — and even lists it
/// as REQUIRED. So deriving `JsonSchema` on the real struct is NOT
/// semantics-free: the schema gains a required property the command never
/// emits, and `doctor schemas` (keys-in-sample only) would not catch it. The
/// custom emitter, walking the serialized field set, omits it by construction.
#[test]
fn schemars_re_exposes_skip_serialized_verification_field() {
    let schemars_schema = schemars_path::schema();
    let schemars_props: BTreeSet<String> = property_keys(&schemars_schema).into_iter().collect();
    let custom_props: BTreeSet<String> =
        property_keys(&custom_path::schema()).into_iter().collect();
    let wire_keys = serialized_example_keys();

    assert!(
        schemars_props.contains("verification"),
        "schemars should re-expose the skip_serializing `verification` field; \
         got props: {schemars_props:?}"
    );
    let required: BTreeSet<String> = schemars_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        required.contains("verification"),
        "schemars marks the phantom `verification` field REQUIRED — the drift is \
         worse than optional; got required: {required:?}"
    );
    assert!(
        !wire_keys.contains("verification"),
        "the serialized init output must NOT carry `verification` (it is \
         skip_serializing); got wire keys: {wire_keys:?}"
    );
    assert!(
        !custom_props.contains("verification"),
        "the custom emitter walks the serialized field set and must omit \
         `verification`; got props: {custom_props:?}"
    );
    // The drift is exactly this one phantom property.
    let schemars_minus_wire: BTreeSet<&String> = schemars_props.difference(&wire_keys).collect();
    assert_eq!(
        schemars_minus_wire,
        BTreeSet::from([&"verification".to_string()]),
        "schemars schema vs wire output should differ by exactly `verification`"
    );
}

/// Spike answer Q1/examples — the typed example diverges from the curated
/// `docs/json-schemas.md` sample, ASSERTED. The real `InitOutput` always
/// serializes the principal fields (no `skip_serializing_if`), so a faithful
/// typed example carries keys the hand-curated prose sample omits. This is the
/// evidence for "typed examples beat prose samples": the example cannot drift
/// from the struct, but the prose sample already has. heddle#205 must rebaseline
/// the documented sample to the real shape.
#[test]
fn typed_example_diverges_from_curated_doc_sample() {
    let documented: BTreeSet<String> = documented_sample_keys()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let wire_keys = serialized_example_keys();

    // The real output is a superset of the documented sample's keys.
    assert!(
        documented.is_subset(&wire_keys),
        "every documented key must be present in the real output; documented: \
         {documented:?}, wire: {wire_keys:?}"
    );

    // …and carries exactly these extra always-serialized fields the curated
    // sample drops. Pinning the set turns Codex's "example diverges from docs"
    // finding into a measured regression guard.
    let extra: BTreeSet<String> = wire_keys.difference(&documented).cloned().collect();
    let expected_extra = BTreeSet::from([
        "principal".to_string(),
        "principal_recommended_action".to_string(),
        "principal_source".to_string(),
        "principal_status".to_string(),
    ]);
    assert_eq!(
        extra, expected_extra,
        "typed example vs curated doc sample should differ by exactly the \
         always-serialized principal fields"
    );
}

/// Print the comparison table the spike doc cites. Not an assertion — a probe;
/// the gap and drift it shows are asserted by the dedicated tests above.
#[test]
fn print_measurement_table() {
    let s = schemars_path::schema();
    let c = custom_path::schema();

    let s_pretty = serde_json::to_string_pretty(&s).unwrap();
    let c_pretty = serde_json::to_string_pretty(&c).unwrap();

    let s_keys = property_keys(&s).len();
    let c_keys = property_keys(&c).len();

    let s_has_defs = s.get("definitions").is_some() || s.get("$defs").is_some();
    let s_const = output_kind_pinned(&s);
    let c_const = output_kind_pinned(&c);
    let s_props: BTreeSet<String> = property_keys(&s).into_iter().collect();
    let s_has_verification = s_props.contains("verification");

    println!("\n=== heddle#327 PoC measurement — `init` verb, both ways ===");
    println!("{:<36} | {:>10} | {:>10}", "metric", "schemars", "custom");
    println!("{:-<36}-+-{:-<10}-+-{:-<10}", "", "", "");
    println!(
        "{:<36} | {:>10} | {:>10}",
        "pretty-printed schema bytes",
        s_pretty.len(),
        c_pretty.len()
    );
    println!(
        "{:<36} | {:>10} | {:>10}",
        "top-level property keys", s_keys, c_keys
    );
    println!(
        "{:<36} | {:>10} | {:>10}",
        "uses $ref/definitions (nested)", s_has_defs, false
    );
    println!(
        "{:<36} | {:>10} | {:>10}",
        "output_kind pinned (const/enum)", s_const, c_const
    );
    println!(
        "{:<36} | {:>10} | {:>10}",
        "phantom `verification` property", s_has_verification, false
    );
    println!("{:<36} | {:>10} | {:>10}", "carries example", true, true);
    println!(
        "{:<36} | {:>10} | {:>10}",
        "needs schemars dep", true, false
    );
    println!("===========================================================\n");

    println!("--- schemars schema ---\n{s_pretty}\n");
    println!("--- custom-emitter schema ---\n{c_pretty}\n");
}
