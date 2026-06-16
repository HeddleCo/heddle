// SPDX-License-Identifier: Apache-2.0
//! Generate TypeScript types from heddle's runtime JSON-Schema introspection.
//!
//! This is the source of truth for the `clients/npm` wrapper (#581/#584): it
//! walks every schema verb ([`schema_verbs`]), renders the schemars-derived
//! JSON Schema for each ([`schema_for_verb`]), and produces both the raw
//! schemas and hand-free TypeScript declarations a Node/Electron harness can
//! import.
//!
//! The output is deterministic (everything sorted), so regenerating on an
//! unchanged contract produces a no-op diff. The `gen_ts_types` example writes
//! it to disk; `tests/ts_types_in_sync.rs` asserts the checked-in files match.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use serde_json::{Map, Value};

use crate::cli::commands::{schema_for_verb, schema_verbs};

/// Schema-contract version the emitted types are pinned to. Tracks the
/// `heddle-cli` crate version: a contract change ships in a new CLI release,
/// so the crate version is the coarsest-correct pin for "which heddle do
/// these types describe".
pub const SCHEMA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The generated wrapper artifacts.
pub struct Generated {
    /// `heddle-schemas.ts` — TypeScript types + verb map + version pin.
    pub typescript: String,
    /// `heddle-schemas.json` — raw JSON Schemas keyed by verb.
    pub json: String,
}

/// Render the TypeScript module and raw-schema JSON from the live catalog.
pub fn generate() -> Generated {
    let mut verbs: Vec<&str> = schema_verbs().to_vec();
    verbs.sort_unstable();
    let verb_schemas: Vec<(String, Value)> = verbs
        .into_iter()
        .filter_map(|verb| schema_for_verb(verb).map(|schema| (verb.to_string(), schema)))
        .collect();
    generate_from(verb_schemas)
}

/// A single global name registry that every emitted type — `$def` *and* root —
/// flows through, so no two distinct bodies can ever share a sanitized name.
///
/// This closes the name-collision class structurally: defs and roots are not
/// two separate namespaces that can clobber each other, they are one allocation
/// pass. When two distinct bodies want the same sanitized name (root-vs-root,
/// root-vs-def, or def-vs-def across verbs) the later one is disambiguated with
/// a numeric suffix instead of overwriting, and every `$ref` that pointed at the
/// renamed def is rewritten to its allocated name so it still resolves to the
/// intended definition.
struct NameRegistry {
    /// final emitted name -> type body.
    types: BTreeMap<String, Value>,
    /// desired base name -> allocated `(final_name, closure_signature)` pairs.
    /// A new request reuses an existing final name only when its full ref
    /// closure is byte-identical (i.e. genuinely the same type); otherwise it
    /// gets a fresh, suffixed name.
    by_base: BTreeMap<String, Vec<(String, String)>>,
}

impl NameRegistry {
    fn new() -> Self {
        Self {
            types: BTreeMap::new(),
            by_base: BTreeMap::new(),
        }
    }

    /// Reserve a unique final name for `base`. `sig` is the body's ref-closure
    /// signature: identical signatures dedup to one shared type, distinct ones
    /// are kept apart. Returns `(final_name, is_new)`; when `is_new` the caller
    /// must store the (ref-rewritten) body under `final_name`.
    fn allocate(&mut self, base: &str, sig: &str) -> (String, bool) {
        if let Some(existing) = self.by_base.get(base) {
            for (final_name, existing_sig) in existing {
                if existing_sig == sig {
                    return (final_name.clone(), false);
                }
            }
        }
        let mut candidate = base.to_string();
        let mut n = 1;
        while self.types.contains_key(&candidate) {
            n += 1;
            candidate = format!("{base}{n}");
        }
        // Claim the name immediately so concurrent allocations in the same pass
        // can't pick it; the real body is written by the caller when new.
        self.types.insert(candidate.clone(), Value::Null);
        self.by_base
            .entry(base.to_string())
            .or_default()
            .push((candidate.clone(), sig.to_string()));
        (candidate, true)
    }
}

/// Core codegen over an explicit `(verb, schema)` list. Split out from
/// [`generate`] so collision handling is unit-testable without the live
/// catalog.
fn generate_from(verb_schemas: Vec<(String, Value)>) -> Generated {
    // Deterministic processing order so suffix assignment is stable.
    let mut verb_schemas = verb_schemas;
    verb_schemas.sort_by(|a, b| a.0.cmp(&b.0));

    let mut registry = NameRegistry::new();
    let mut verb_to_type: BTreeMap<String, String> = BTreeMap::new();
    let mut raw: BTreeMap<String, Value> = BTreeMap::new();

    // A title shared by >1 verb can't name both verbs' root types, so the
    // colliding verbs each fall back to a distinct per-verb type name.
    let mut title_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (verb, schema) in &verb_schemas {
        let title = root_title(verb, schema);
        *title_counts.entry(title).or_default() += 1;
    }

    for (verb, schema) in &verb_schemas {
        // 1. This verb's `$defs`, keyed by their ORIGINAL (unsanitized) names —
        //    the stable unique key. `$ref`s resolve by original name too, so two
        //    defs that *sanitize* to the same identifier (e.g. `Foo-Bar` and
        //    `Foo_Bar`) stay distinct here and are disambiguated at allocation
        //    instead of one silently clobbering the other.
        let mut defs: BTreeMap<String, Value> = BTreeMap::new();
        if let Some(obj) = schema.get("$defs").and_then(Value::as_object) {
            for (name, body) in obj {
                defs.insert(name.clone(), body.clone());
            }
        }

        // 2. Allocate a global name for every def, building this verb's
        //    rename map (original def name -> final emitted name). The desired
        //    base is the sanitized form; collisions are suffixed, never dropped.
        let mut rename: BTreeMap<String, String> = BTreeMap::new();
        let mut newly: Vec<(String, String)> = Vec::new();
        for name in defs.keys() {
            let sig = body_sig(&defs[name], &defs);
            let (final_name, is_new) = registry.allocate(&sanitize_ident(name), &sig);
            if is_new {
                newly.push((name.clone(), final_name.clone()));
            }
            rename.insert(name.clone(), final_name);
        }

        // 3. Store each newly-allocated def body with its `$ref`s rewritten to
        //    the verb's resolved names. Shared (deduped) defs are already
        //    present and structurally identical, so they are left untouched.
        for (orig, final_name) in &newly {
            let mut body = defs[orig].clone();
            rewrite_refs(&mut body, &rename);
            registry.types.insert(final_name.clone(), body);
        }

        // 4. The root: same registry, so it can never overwrite a def of a
        //    different shape — a collision suffixes the root instead.
        let title = root_title(verb, schema);
        let desired = if title_counts.get(&title).copied().unwrap_or(0) > 1 {
            verb_type_name(verb)
        } else {
            title
        };
        let mut root_body = strip_root_meta(schema);
        // Sign the root by its *content* (raw refs, like the defs above) so a
        // root that is genuinely the same shape as a same-named `$def` shares
        // one type, while a root that differs (e.g. carries a runtime-injected
        // discriminator the `$def` lacks) gets a distinct, suffixed name instead
        // of overwriting the `$def`.
        let root_sig = body_sig(&root_body, &defs);
        rewrite_refs(&mut root_body, &rename);
        let (final_name, is_new) = registry.allocate(&desired, &root_sig);
        if is_new {
            registry.types.insert(final_name.clone(), root_body);
        }
        verb_to_type.insert(verb.clone(), final_name);

        raw.insert(verb.clone(), schema.clone());
    }

    let types = registry.types;
    let typescript = render_ts(&types, &verb_to_type);
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "schemaVersion": SCHEMA_VERSION,
        "verbs": raw,
    }))
    .expect("raw schemas serialize")
        + "\n";

    Generated { typescript, json }
}

/// The sanitized type name a verb's root *wants*, before collision handling:
/// its schema `title`, or a verb-derived name when the schema has no title.
fn root_title(verb: &str, schema: &Value) -> String {
    schema
        .get("title")
        .and_then(Value::as_str)
        .map(sanitize_ident)
        .unwrap_or_else(|| verb_type_name(verb))
}

/// Recursively rewrite every `$ref` so its terminal name resolves through
/// `rename` (original def name -> final emitted name). Every intra-verb ref is
/// in the map, so all are rewritten to their allocated name; refs to unknown
/// targets keep their original form. This keeps `$ref`s pointing at the right
/// definition after a colliding def was given a suffixed name.
fn rewrite_refs(value: &mut Value, rename: &BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            let remapped = map.get("$ref").and_then(Value::as_str).and_then(|r| {
                let terminal = r.rsplit('/').next().unwrap_or(r);
                rename
                    .get(terminal)
                    .map(|final_name| format!("#/$defs/{final_name}"))
            });
            if let Some(new_ref) = remapped {
                map.insert("$ref".to_string(), Value::String(new_ref));
            }
            for v in map.values_mut() {
                rewrite_refs(v, rename);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                rewrite_refs(v, rename);
            }
        }
        _ => {}
    }
}

/// A deterministic signature of a type body's *full ref closure* within one
/// verb's `$defs`. Two bodies (def or root) share a name only when their
/// signatures match — i.e. they are the same type all the way down — so a
/// shallow same-name/same-body coincidence whose nested refs differ is never
/// wrongly merged, and a root identical to a same-named `$def` is unified rather
/// than duplicated.
fn body_sig(body: &Value, defs: &BTreeMap<String, Value>) -> String {
    let mut visited = BTreeSet::new();
    let mut buf = String::new();
    sig_value(body, defs, &mut visited, &mut buf);
    buf
}

fn sig_node(
    name: &str,
    defs: &BTreeMap<String, Value>,
    visited: &mut BTreeSet<String>,
    buf: &mut String,
) {
    if !visited.insert(name.to_string()) {
        // Back-reference into a cycle: record the name, don't re-expand.
        buf.push('@');
        buf.push_str(name);
        buf.push(';');
        return;
    }
    match defs.get(name) {
        Some(body) => {
            buf.push_str(name);
            buf.push('=');
            sig_value(body, defs, visited, buf);
            buf.push(';');
        }
        // Ref into another verb / unknown def: name alone, can't expand.
        None => {
            buf.push('?');
            buf.push_str(name);
            buf.push(';');
        }
    }
}

fn sig_value(
    value: &Value,
    defs: &BTreeMap<String, Value>,
    visited: &mut BTreeSet<String>,
    buf: &mut String,
) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                let terminal = reference.rsplit('/').next().unwrap_or(reference);
                buf.push_str("ref(");
                sig_node(terminal, defs, visited, buf);
                buf.push(')');
                return;
            }
            buf.push('{');
            for (k, v) in map {
                buf.push_str(k);
                buf.push(':');
                sig_value(v, defs, visited, buf);
                buf.push(',');
            }
            buf.push('}');
        }
        Value::Array(items) => {
            buf.push('[');
            for v in items {
                sig_value(v, defs, visited, buf);
                buf.push(',');
            }
            buf.push(']');
        }
        other => buf.push_str(&other.to_string()),
    }
}

/// Drop the JSON-Schema envelope keys, keeping only the type body so a root
/// can be emitted exactly like a `$def`.
fn strip_root_meta(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (k, v) in obj {
        if matches!(k.as_str(), "$schema" | "$defs" | "title") {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

fn render_ts(types: &BTreeMap<String, Value>, verb_to_type: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    out.push_str(
        "// GENERATED by `cargo run -p heddle-cli --example gen_ts_types` — DO NOT EDIT.\n",
    );
    out.push_str("// Source of truth: heddle's runtime JSON-Schema introspection\n");
    out.push_str("// (`heddle schemas <verb>` / `crates/cli/src/cli/commands/schemas.rs`).\n");
    out.push_str(
        "// Regenerate with `scripts/gen-ts-types.sh`; a drift test keeps it in sync.\n\n",
    );
    let _ = writeln!(
        out,
        "export const HEDDLE_SCHEMA_VERSION = {:?} as const;\n",
        SCHEMA_VERSION
    );

    for (name, body) in types {
        emit_type(&mut out, name, body);
    }

    out.push_str("/** Maps each `--output json` verb to its output payload type. */\n");
    out.push_str("export interface HeddleVerbOutputs {\n");
    for (verb, ty) in verb_to_type {
        let _ = writeln!(out, "  {}: {};", quote_key(verb), ty);
    }
    out.push_str("}\n\n");

    out.push_str("/** Every verb that emits a schema-backed `--output json` payload. */\n");
    out.push_str("export type HeddleSchemaVerb = keyof HeddleVerbOutputs;\n\n");

    out.push_str("export const HEDDLE_SCHEMA_VERBS: readonly HeddleSchemaVerb[] = [\n");
    for verb in verb_to_type.keys() {
        let _ = writeln!(out, "  {},", json_string(verb));
    }
    out.push_str("] as const;\n");

    out
}

fn emit_type(out: &mut String, name: &str, body: &Value) {
    let is_object = body.get("type").and_then(Value::as_str) == Some("object")
        && body.get("properties").is_some();

    if let Some(desc) = body.get("description").and_then(Value::as_str) {
        emit_jsdoc(out, desc, "");
    }

    if is_object {
        let _ = writeln!(out, "export interface {name} {{");
        emit_object_body(out, body, "  ");
        out.push_str("}\n\n");
    } else {
        let _ = writeln!(out, "export type {name} = {};\n", ts_type(body));
    }
}

/// Emit the fields of an object schema into an already-open `{ ... }` block.
fn emit_object_body(out: &mut String, body: &Value, indent: &str) {
    let required: Vec<&str> = body
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    if let Some(props) = body.get("properties").and_then(Value::as_object) {
        for (field, schema) in props {
            if let Some(desc) = schema.get("description").and_then(Value::as_str) {
                emit_jsdoc(out, desc, indent);
            }
            let opt = if required.contains(&field.as_str()) {
                ""
            } else {
                "?"
            };
            let _ = writeln!(
                out,
                "{indent}{}{opt}: {};",
                quote_key(field),
                ts_type(schema)
            );
        }
    }

    // Open record / flattened-map shapes.
    match body.get("additionalProperties") {
        Some(Value::Bool(true)) => {
            let _ = writeln!(out, "{indent}[key: string]: unknown;");
        }
        Some(v @ Value::Object(_)) => {
            let _ = writeln!(out, "{indent}[key: string]: {};", ts_type(v));
        }
        _ => {}
    }
}

/// Convert a JSON-Schema node into a TypeScript type expression.
fn ts_type(node: &Value) -> String {
    match node {
        Value::Bool(true) => return "unknown".to_string(),
        Value::Bool(false) => return "never".to_string(),
        _ => {}
    }
    let Some(obj) = node.as_object() else {
        return "unknown".to_string();
    };

    if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
        return ref_name(reference);
    }

    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        let mut parts: Vec<String> = values.iter().map(literal).collect();
        parts.dedup();
        return parts.join(" | ");
    }

    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = obj.get(key).and_then(Value::as_array) {
            let mut parts: Vec<String> = variants.iter().map(ts_type).collect();
            parts.dedup();
            return union(parts);
        }
    }

    if let Some(all) = obj.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = all.iter().map(ts_type).collect();
        return parts.join(" & ");
    }

    match obj.get("type") {
        Some(Value::String(t)) => ts_scalar(t, obj),
        Some(Value::Array(kinds)) => {
            let mut parts: Vec<String> = kinds
                .iter()
                .filter_map(Value::as_str)
                .map(|t| ts_scalar(t, obj))
                .collect();
            parts.dedup();
            union(parts)
        }
        _ => {
            if obj.contains_key("properties") || obj.contains_key("additionalProperties") {
                inline_object(obj)
            } else {
                "unknown".to_string()
            }
        }
    }
}

fn ts_scalar(t: &str, obj: &Map<String, Value>) -> String {
    match t {
        "string" => "string".to_string(),
        "integer" | "number" => "number".to_string(),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => {
            let item = obj
                .get("items")
                .map(ts_type)
                .unwrap_or_else(|| "unknown".to_string());
            if item.contains(' ') || item.contains('|') || item.contains('&') {
                format!("({item})[]")
            } else {
                format!("{item}[]")
            }
        }
        "object" => inline_object(obj),
        other => format!("unknown /* {other} */"),
    }
}

fn inline_object(obj: &Map<String, Value>) -> String {
    if obj.get("properties").and_then(Value::as_object).is_none() {
        return match obj.get("additionalProperties") {
            Some(v @ Value::Object(_)) => format!("Record<string, {}>", ts_type(v)),
            _ => "Record<string, unknown>".to_string(),
        };
    }
    let body = Value::Object(obj.clone());
    let mut inner = String::new();
    emit_object_body(&mut inner, &body, "");
    let fields: Vec<&str> = inner
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    format!("{{ {} }}", fields.join(" "))
}

fn union(mut parts: Vec<String>) -> String {
    parts.retain(|p| !p.is_empty());
    if parts.is_empty() {
        return "unknown".to_string();
    }
    parts.join(" | ")
}

fn ref_name(reference: &str) -> String {
    let raw = reference.rsplit('/').next().unwrap_or(reference);
    sanitize_ident(raw)
}

fn literal(v: &Value) -> String {
    match v {
        Value::String(s) => json_string(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => json_string(&other.to_string()),
    }
}

fn json_string(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// Object keys that are valid bare TS identifiers stay bare; everything else
/// (verbs with spaces, etc.) gets quoted.
fn quote_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()));
    if bare {
        key.to_string()
    } else {
        json_string(key)
    }
}

fn sanitize_ident(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn verb_type_name(verb: &str) -> String {
    let camel: String = verb
        .split([' ', '-', '_'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    format!("{camel}Schema")
}

fn emit_jsdoc(out: &mut String, desc: &str, indent: &str) {
    let one_line = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    let safe = one_line.replace("*/", "*\\/");
    let _ = writeln!(out, "{indent}/** {safe} */");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Two verbs whose schemas share a `title` must each emit their own root
    /// body — neither overwritten. Regression guard for the title-keyed roots
    /// map that dropped all-but-the-last verb's root.
    #[test]
    fn shared_title_preserves_each_verbs_root_body() {
        let schema_a = json!({
            "title": "SharedTitle",
            "type": "object",
            "properties": { "alpha": { "type": "string" } },
            "required": ["alpha"],
        });
        let schema_b = json!({
            "title": "SharedTitle",
            "type": "object",
            "properties": { "beta": { "type": "number" } },
            "required": ["beta"],
        });

        let generated = generate_from(vec![
            ("verb_a".to_string(), schema_a),
            ("verb_b".to_string(), schema_b),
        ]);
        let ts = &generated.typescript;

        // Both verbs' distinct fields survive — the earlier root isn't clobbered.
        assert!(ts.contains("alpha"), "verb_a root body missing:\n{ts}");
        assert!(ts.contains("beta"), "verb_b root body missing:\n{ts}");
        // And each verb is mapped to a type in the verb->payload map.
        assert!(ts.contains("verb_a:"), "verb_a not mapped:\n{ts}");
        assert!(ts.contains("verb_b:"), "verb_b not mapped:\n{ts}");
    }

    /// Conformance guard for the full collision class across BOTH namespaces:
    /// (a) two verbs share a root `title`, and (b) a third verb's root name
    /// collides with a `$def` emitted by another verb. Every distinct type must
    /// survive and every `$ref` must still resolve to its intended definition —
    /// no global overwrite of the shared `$def`.
    #[test]
    fn root_and_def_name_collisions_emit_distinct_types() {
        // verb_a: title shared with verb_b (root-vs-root), owns a `Widget` $def
        // that its own root references by $ref.
        let schema_a = json!({
            "title": "SharedTitle",
            "type": "object",
            "properties": {
                "alpha": { "type": "string" },
                "widget": { "$ref": "#/$defs/Widget" },
            },
            "required": ["alpha", "widget"],
            "$defs": {
                "Widget": {
                    "type": "object",
                    "properties": { "gamma": { "type": "string" } },
                    "required": ["gamma"],
                },
            },
        });
        // verb_b: same title as verb_a — root-vs-root collision.
        let schema_b = json!({
            "title": "SharedTitle",
            "type": "object",
            "properties": { "beta": { "type": "number" } },
            "required": ["beta"],
        });
        // verb_c: its root title sanitizes to `Widget`, colliding with verb_a's
        // $def name — but it's a different shape (carries `delta`, not `gamma`).
        let schema_c = json!({
            "title": "Widget",
            "type": "object",
            "properties": { "delta": { "type": "boolean" } },
            "required": ["delta"],
        });

        let generated = generate_from(vec![
            ("verb_c".to_string(), schema_c),
            ("verb_a".to_string(), schema_a),
            ("verb_b".to_string(), schema_b),
        ]);
        let ts = &generated.typescript;

        // Root-vs-root: both shared-title verbs keep their own distinct body.
        assert!(ts.contains("alpha"), "verb_a root body missing:\n{ts}");
        assert!(ts.contains("beta"), "verb_b root body missing:\n{ts}");

        // The shared `$def` is emitted intact and is NOT overwritten by verb_c's
        // same-named root — `gamma` (the def) and `delta` (the root) coexist as
        // separate types.
        assert!(
            ts.contains("export interface Widget {"),
            "Widget $def missing:\n{ts}"
        );
        let widget_def = ts
            .split("export interface Widget {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or("");
        assert!(
            widget_def.contains("gamma") && !widget_def.contains("delta"),
            "Widget $def was overwritten by verb_c's root:\n{ts}"
        );

        // verb_a's $ref still resolves to the `Widget` $def (not verb_c's root).
        assert!(
            ts.contains("widget: Widget;"),
            "verb_a root $ref no longer resolves to the Widget def:\n{ts}"
        );

        // verb_c's colliding root got a distinct name carrying its own `delta`,
        // and is mapped — proving it wasn't silently dropped onto the $def.
        let verb_c_type = generated
            .typescript
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("verb_c: ")
                    .map(|t| t.trim_end_matches(';').to_string())
            })
            .expect("verb_c mapped");
        assert_ne!(
            verb_c_type, "Widget",
            "verb_c root collided onto the $def name:\n{ts}"
        );
        let verb_c_def = ts
            .split(&format!("export interface {verb_c_type} {{"))
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or("");
        assert!(
            verb_c_def.contains("delta"),
            "verb_c root body ({verb_c_type}) missing its own field:\n{ts}"
        );
    }

    /// Definitive close-the-class guard: every collision sub-case in ONE run,
    /// including the one that drips kept reappearing —
    ///   (a) two verbs share a root `title` (root-vs-root),
    ///   (b) a verb's root name collides with another verb's `$def` (root-vs-def),
    ///   (c) two `$defs` WITHIN one schema sanitize to the same identifier
    ///       (`Foo-Bar` + `Foo_Bar`, def-vs-def intra-schema).
    /// Because name allocation is keyed by each def's *original* name (not its
    /// sanitized form) and disambiguates on collision, no body is ever dropped
    /// and every `$ref` resolves to its intended type.
    #[test]
    fn all_name_collision_subcases_emit_distinct_types() {
        // verb_a: shares title with verb_b (a); owns a `Widget` $def that verb_c
        // will collide with (b); AND two intra-schema defs that sanitize to the
        // same ident with DISTINCT bodies, each referenced by the root (c).
        let schema_a = json!({
            "title": "SharedTitle",
            "type": "object",
            "properties": {
                "alpha": { "type": "string" },
                "widget": { "$ref": "#/$defs/Widget" },
                "fooDash": { "$ref": "#/$defs/Foo-Bar" },
                "fooUnder": { "$ref": "#/$defs/Foo_Bar" },
            },
            "required": ["alpha", "widget", "fooDash", "fooUnder"],
            "$defs": {
                "Widget": {
                    "type": "object",
                    "properties": { "gamma": { "type": "string" } },
                    "required": ["gamma"],
                },
                "Foo-Bar": {
                    "type": "object",
                    "properties": { "dashField": { "type": "string" } },
                    "required": ["dashField"],
                },
                "Foo_Bar": {
                    "type": "object",
                    "properties": { "underField": { "type": "number" } },
                    "required": ["underField"],
                },
            },
        });
        let schema_b = json!({
            "title": "SharedTitle",
            "type": "object",
            "properties": { "beta": { "type": "number" } },
            "required": ["beta"],
        });
        let schema_c = json!({
            "title": "Widget",
            "type": "object",
            "properties": { "delta": { "type": "boolean" } },
            "required": ["delta"],
        });

        let generated = generate_from(vec![
            ("verb_c".to_string(), schema_c),
            ("verb_a".to_string(), schema_a),
            ("verb_b".to_string(), schema_b),
        ]);
        let ts = &generated.typescript;

        let iface_body = |name: &str| -> String {
            ts.split(&format!("export interface {name} {{"))
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .unwrap_or("")
                .to_string()
        };
        let verb_type = |verb: &str| -> String {
            ts.lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix(&format!("{verb}: "))
                        .map(|t| t.trim_end_matches(';').to_string())
                })
                .unwrap_or_else(|| panic!("{verb} not mapped:\n{ts}"))
        };

        // (a) root-vs-root: both shared-title verbs keep their own body.
        assert!(ts.contains("alpha"), "verb_a root body missing:\n{ts}");
        assert!(ts.contains("beta"), "verb_b root body missing:\n{ts}");
        assert_ne!(
            verb_type("verb_a"),
            verb_type("verb_b"),
            "roots collapsed:\n{ts}"
        );

        // (b) root-vs-def: the `Widget` $def survives intact, verb_c's same-named
        // root got a distinct name, and verb_a's $ref still points at the def.
        assert!(
            iface_body("Widget").contains("gamma") && !iface_body("Widget").contains("delta"),
            "Widget $def overwritten by verb_c root:\n{ts}"
        );
        assert_ne!(
            verb_type("verb_c"),
            "Widget",
            "verb_c root collided onto the def:\n{ts}"
        );
        assert!(
            iface_body(&verb_type("verb_c")).contains("delta"),
            "verb_c body lost:\n{ts}"
        );

        // (c) def-vs-def intra-schema: BOTH `Foo-Bar` and `Foo_Bar` are emitted as
        // distinct types (one suffixed), neither dropped, and the root's two refs
        // resolve to the correct one each.
        let a_root = iface_body(&verb_type("verb_a"));
        let dash_ty = a_root
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("fooDash: ")
                    .map(|t| t.trim_end_matches(';').to_string())
            })
            .expect("fooDash field present");
        let under_ty = a_root
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("fooUnder: ")
                    .map(|t| t.trim_end_matches(';').to_string())
            })
            .expect("fooUnder field present");
        assert_ne!(
            dash_ty, under_ty,
            "two intra-schema defs collapsed to one type:\n{ts}"
        );
        assert!(
            iface_body(&dash_ty).contains("dashField"),
            "fooDash ({dash_ty}) resolved to the wrong def:\n{ts}"
        );
        assert!(
            iface_body(&under_ty).contains("underField"),
            "fooUnder ({under_ty}) resolved to the wrong def:\n{ts}"
        );
    }
}
