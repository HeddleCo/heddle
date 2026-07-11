// SPDX-License-Identifier: Apache-2.0
//! Pure schema-doc sample extraction and drift helpers for `heddle doctor schemas`.
//!
//! Owns markdown sample parsing, JSON Schema property-key walks, and
//! coverage-value comparison that need only `serde_json::Value` / strings.
//! Filesystem reads, RecoveryAdvice, clap, and catalog registry stay CLI-owned.

use std::collections::BTreeSet;

use serde_json::Value;

/// A literal JSON sample lifted out of `docs/json-schemas.md`.
#[derive(Debug, Clone)]
pub struct DocSample {
    /// The closest preceding `## ` heading, used as the fallback
    /// verb when no inline `heddle <verb>` reference is present.
    pub heading: String,
    /// Inline verb reference parsed from the most recent paragraph
    /// before the fence (e.g. `` `heddle thread marker create|delete|show`
    /// emit: ``). When present, this overrides the section heading.
    pub inline_verb: Option<String>,
    /// 1-based line number where the ```json fence opens.
    pub start_line: usize,
    /// Parsed JSON. May be a non-object (e.g. literal `null`).
    pub json: Value,
}

/// One field-level drift between a documented coverage object and runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageFieldDrift {
    /// Dotted path used as the issue key (e.g. `command_contract_schema_coverage.summary`).
    pub field_path: String,
    /// Human detail for the text renderer / SchemaIssue.detail.
    pub detail: String,
}

/// Numeric coverage facts that gate whether schema coverage still blocks verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaCoverageBlockingFacts {
    pub verified_scope_json_commands_without_schema: usize,
    pub verified_scope_mutating_commands_without_schema: usize,
    pub verified_scope_json_commands_with_accepted_opaque_schema: usize,
    pub verified_scope_mutating_commands_with_accepted_opaque_schema: usize,
    pub unaccepted_opaque_schema_verbs_total: usize,
    pub undocumented_schema_verbs_total: usize,
}

/// True when verified-scope schema coverage has no blocking gaps.
pub fn coverage_has_no_blocking_schema_gaps(facts: SchemaCoverageBlockingFacts) -> bool {
    facts.verified_scope_json_commands_without_schema == 0
        && facts.verified_scope_mutating_commands_without_schema == 0
        && facts.verified_scope_json_commands_with_accepted_opaque_schema == 0
        && facts.verified_scope_mutating_commands_with_accepted_opaque_schema == 0
        && facts.unaccepted_opaque_schema_verbs_total == 0
        && facts.undocumented_schema_verbs_total == 0
}

/// Byte span of the JSON body under `## \`heddle doctor schemas --output json\``.
///
/// Returns `(content_start, content_end)` indices into `doc` (exclusive end).
pub fn doctor_schemas_json_sample_span(doc: &str) -> Result<(usize, usize), JsonSampleSpanError> {
    let heading = "## `heddle doctor schemas --output json`";
    let heading_start = doc
        .find(heading)
        .ok_or(JsonSampleSpanError::MissingHeading)?;
    let after_heading = &doc[heading_start..];
    let fence_rel = after_heading
        .find("```json")
        .ok_or(JsonSampleSpanError::MissingFence)?;
    let fence_start = heading_start + fence_rel;
    let content_start = doc[fence_start..]
        .find('\n')
        .map(|newline| fence_start + newline + 1)
        .ok_or(JsonSampleSpanError::UnterminatedFence)?;
    let content_end = doc[content_start..]
        .find("\n```")
        .map(|closing| content_start + closing)
        .ok_or(JsonSampleSpanError::UnterminatedFence)?;
    Ok((content_start, content_end))
}

/// Errors locating the doctor-schemas JSON sample fence in markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonSampleSpanError {
    MissingHeading,
    MissingFence,
    UnterminatedFence,
}

impl JsonSampleSpanError {
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingHeading => "missing `## heddle doctor schemas --output json` section",
            Self::MissingFence => {
                "missing JSON fence under `## heddle doctor schemas --output json`"
            }
            Self::UnterminatedFence => {
                "unterminated JSON fence under `## heddle doctor schemas --output json`"
            }
        }
    }
}

/// Top-level property keys declared in a generated schema.
///
/// Returns an empty set when `properties` is missing (e.g. the schema is a
/// `null` or a primitive).
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

/// Whether the schema permits undeclared top-level properties.
pub fn schema_allows_additional_properties(schema: &Value) -> bool {
    schema
        .get("additionalProperties")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Walk every fenced ```json block in the doc and pair it with both
/// the nearest preceding `## ` heading and the most recent inline
/// `heddle <verb>` reference. Skips fences whose JSON doesn't parse
/// — those are rare placeholder snippets (e.g. samples with `...`
/// fillers) that we deliberately don't validate.
pub fn extract_samples(doc: &str) -> Vec<DocSample> {
    let mut samples = Vec::new();
    let mut current_heading = String::new();
    let mut last_inline_verb: Option<String> = None;
    let mut in_fence = false;
    let mut fence_start = 0usize;
    let mut buffer = String::new();

    for (idx, line) in doc.lines().enumerate() {
        let lineno = idx + 1;
        if !in_fence && line.starts_with("## ") {
            current_heading = line.trim_start_matches("## ").trim().to_string();
            // New section — drop any stale inline verb hint.
            last_inline_verb = None;
            continue;
        }
        if !in_fence {
            // Look for inline verb mentions in normal prose, e.g.
            // `` `heddle thread marker create|delete|show` emit: ``.
            if let Some(verb) = parse_inline_verb(line) {
                last_inline_verb = Some(verb);
            }
        }
        if !in_fence && line.trim() == "```json" {
            in_fence = true;
            fence_start = lineno;
            buffer.clear();
            continue;
        }
        if in_fence && line.trim() == "```" {
            in_fence = false;
            // Try to parse — silently skip placeholder samples
            // that contain `...` ellipses or other non-JSON.
            if let Ok(json) = serde_json::from_str::<Value>(&buffer) {
                samples.push(DocSample {
                    heading: current_heading.clone(),
                    inline_verb: last_inline_verb.clone(),
                    start_line: fence_start,
                    json,
                });
                // Clear the inline hint once consumed so it doesn't
                // bleed onto the next sample.
                last_inline_verb = None;
            }
            buffer.clear();
            continue;
        }
        if in_fence {
            buffer.push_str(line);
            buffer.push('\n');
        }
    }

    samples
}

/// Every `docs/json-schemas.md` ```json sample that binds to at least one
/// verb in `verbs`, paired with the subset of `verbs` it binds to (via
/// section heading or inline `heddle <verb>` hint), in document order.
pub fn documented_samples_with_bound_verbs(doc: &str, verbs: &[&str]) -> Vec<(Value, Vec<String>)> {
    extract_samples(doc)
        .into_iter()
        .filter_map(|sample| {
            let bound: Vec<String> = verbs
                .iter()
                .filter(|verb| sample_matches_verb_with_hints(&sample, verb))
                .map(|verb| (*verb).to_string())
                .collect();
            (!bound.is_empty()).then_some((sample.json, bound))
        })
        .collect()
}

/// Bind a sample to a verb using inline hints first, then heading fallback.
pub fn sample_matches_verb_with_hints(sample: &DocSample, verb: &str) -> bool {
    if let Some(inline) = &sample.inline_verb {
        if inline_verb_matches(inline, verb) {
            return true;
        }
        // When an inline hint is present, do *not* fall back to the
        // section heading — the inline hint is more specific and
        // overrides.
        return false;
    }
    sample_matches_verb(&sample.heading, verb)
}

/// Match a `verb` against an inline reference like `marker create|delete|show`.
pub fn inline_verb_matches(inline: &str, verb: &str) -> bool {
    let trimmed = inline.trim();
    if trimmed == verb {
        return true;
    }
    let mut parts: Vec<&str> = trimmed.split_whitespace().collect();
    if let Some(last) = parts.pop()
        && last.contains('|')
    {
        let prefix = parts.join(" ");
        for variant in last.split('|') {
            let combined = if prefix.is_empty() {
                variant.to_string()
            } else {
                format!("{prefix} {variant}")
            };
            if combined == verb {
                return true;
            }
        }
    }
    false
}

/// Parse an inline verb reference out of a single line of prose.
///
/// Handles the canonical doc form: a backtick-fenced `heddle <verb>`
/// followed by `emits:` or `emit:`.
pub fn parse_inline_verb(line: &str) -> Option<String> {
    let backtick_start = line.find('`')?;
    let after_first = &line[backtick_start + 1..];
    let backtick_end_rel = after_first.find('`')?;
    let inner = after_first[..backtick_end_rel].trim();
    let inner_verb = inner.strip_prefix("heddle ").unwrap_or(inner).trim();
    let inner_verb = strip_json_mode_tokens(inner_verb);
    if !is_plausible_verb_phrase(&inner_verb) {
        return None;
    }
    let after_close = &line[backtick_start + 1 + backtick_end_rel + 1..];
    let after_close_lower = after_close.to_ascii_lowercase();
    if after_close_lower.contains("emits") || after_close_lower.contains("emit") {
        Some(inner_verb)
    } else {
        None
    }
}

/// Lightweight plausibility check for an inline-verb candidate.
pub fn is_plausible_verb_phrase(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || matches!(c, ' ' | '-' | '|' | '<' | '>' | '_')
    })
}

/// `## heddle status --output json` matches the verb `"status"`.
pub fn sample_matches_verb(heading: &str, verb: &str) -> bool {
    let stripped = heading.trim_start_matches('`').trim_end_matches('`').trim();
    let stripped = stripped.trim_start_matches("heddle ").trim();
    let mut tokens = strip_json_mode_tokens(stripped)
        .split_whitespace()
        .filter(|tok| !tok.starts_with('<'))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return false;
    }
    let last = tokens.pop().unwrap();
    let prefix = tokens.join(" ");
    if last.contains('|') {
        for variant in last.split('|') {
            let combined = if prefix.is_empty() {
                variant.to_string()
            } else {
                format!("{prefix} {variant}")
            };
            if combined == verb {
                return true;
            }
        }
        false
    } else {
        let combined = if prefix.is_empty() {
            last.to_string()
        } else {
            format!("{prefix} {last}")
        };
        combined == verb
    }
}

/// Strip `--output json` / `--output=json` selectors from a verb phrase.
pub fn strip_json_mode_tokens(input: &str) -> String {
    let mut out = Vec::new();
    let mut tokens = input.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        if token == "--output=json" {
            continue;
        }
        if token == "--output" && tokens.peek().is_some_and(|next| *next == "json") {
            tokens.next();
            continue;
        }
        out.push(token);
    }
    out.join(" ")
}

/// Returns top-level keys when `value` is an object.
pub fn top_level_keys(value: &Value) -> Option<Vec<String>> {
    let object = value.as_object()?;
    Some(object.keys().cloned().collect())
}

/// Walk a sample JSON tree and report field drifts for embedded coverage objects.
pub fn collect_coverage_field_drifts(
    sample_json: &Value,
    machine_coverage: &Value,
    command_coverage: &Value,
) -> Vec<CoverageFieldDrift> {
    let mut issues = Vec::new();
    collect_coverage_field_drifts_at_path(
        sample_json,
        "",
        machine_coverage,
        command_coverage,
        &mut issues,
    );
    issues
}

fn collect_coverage_field_drifts_at_path(
    value: &Value,
    path: &str,
    machine_coverage: &Value,
    command_coverage: &Value,
    issues: &mut Vec<CoverageFieldDrift>,
) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, child) in map {
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match key.as_str() {
            "machine_contract_coverage" => {
                compare_documented_coverage(&child_path, child, machine_coverage, issues);
            }
            "command_contract_schema_coverage" => {
                compare_documented_coverage(&child_path, child, command_coverage, issues);
            }
            _ => {}
        }
        collect_coverage_field_drifts_at_path(
            child,
            &child_path,
            machine_coverage,
            command_coverage,
            issues,
        );
    }
}

/// Compare documented vs runtime coverage object fields at `path`.
pub fn compare_documented_coverage(
    path: &str,
    documented: &Value,
    runtime: &Value,
    issues: &mut Vec<CoverageFieldDrift>,
) {
    let (Value::Object(documented), Value::Object(runtime)) = (documented, runtime) else {
        return;
    };
    for (field, documented_value) in documented {
        let Some(runtime_value) = runtime.get(field) else {
            continue;
        };
        if documented_value != runtime_value {
            let field_path = format!("{path}.{field}");
            issues.push(CoverageFieldDrift {
                field_path: field_path.clone(),
                detail: format!(
                    "sample field '{field_path}' is {}, but runtime reports {}",
                    documented_value, runtime_value
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_samples_from_simple_doc() {
        let doc = "\
## `heddle foo --output json`

Some prose.

```json
{\"a\": 1, \"b\": 2}
```

## `heddle bar --output json`

```json
{\"x\": true}
```
";
        let samples = extract_samples(doc);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].heading, "`heddle foo --output json`");
        assert_eq!(samples[0].json.get("a").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(samples[1].heading, "`heddle bar --output json`");
    }

    #[test]
    fn skips_fences_with_nonparseable_placeholder_samples() {
        let doc = "\
## `heddle baz --output json`

```json
{\"placeholder\": ...}
```
";
        let samples = extract_samples(doc);
        assert!(samples.is_empty());
    }

    #[test]
    fn sample_matches_verb_strips_heddle_prefix_and_args() {
        assert!(sample_matches_verb(
            "`heddle status --output json`",
            "status"
        ));
        assert!(sample_matches_verb(
            "`heddle show <state> --output json`",
            "show"
        ));
        assert!(!sample_matches_verb("`heddle status --output json`", "log"));
    }

    #[test]
    fn inline_verb_pipe_forms_match() {
        assert!(inline_verb_matches(
            "thread marker create|delete|show",
            "thread marker create"
        ));
        assert!(inline_verb_matches(
            "thread marker create|delete|show",
            "thread marker delete"
        ));
        assert!(!inline_verb_matches(
            "thread marker create|delete|show",
            "thread marker list"
        ));
    }

    #[test]
    fn sample_inline_hint_overrides_heading() {
        let sample = DocSample {
            heading: "`heddle thread marker list --output json`".into(),
            inline_verb: Some("thread marker create|delete|show".into()),
            start_line: 10,
            json: serde_json::json!({}),
        };
        assert!(sample_matches_verb_with_hints(
            &sample,
            "thread marker create"
        ));
        assert!(!sample_matches_verb_with_hints(
            &sample,
            "thread marker list"
        ));
    }

    #[test]
    fn top_level_keys_returns_none_for_null() {
        assert!(top_level_keys(&Value::Null).is_none());
        assert!(top_level_keys(&Value::Bool(true)).is_none());
    }

    #[test]
    fn top_level_keys_returns_keys_for_object() {
        let v: Value = serde_json::from_str(r#"{"a": 1, "b": 2}"#).unwrap();
        let mut keys = top_level_keys(&v).unwrap();
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn generic_object_schema_allows_sample_keys() {
        let schema: Value =
            serde_json::from_str(r#"{"type": "object", "additionalProperties": true}"#).unwrap();
        assert!(schema_allows_additional_properties(&schema));
        assert!(schema_property_keys(&schema).is_empty());
    }

    #[test]
    fn schema_property_keys_follow_ref_and_any_of() {
        let schema: Value = serde_json::json!({
            "anyOf": [
                { "$ref": "#/$defs/A" },
                { "properties": { "b": { "type": "string" } } }
            ],
            "$defs": {
                "A": { "properties": { "a": { "type": "integer" } } }
            }
        });
        let keys = schema_property_keys(&schema);
        assert!(keys.contains("a"));
        assert!(keys.contains("b"));
    }

    #[test]
    fn schema_ref_target_decodes_json_pointer_escapes() {
        let root = serde_json::json!({
            "a/b": { "x": 1 },
            "c~d": { "y": 2 }
        });
        assert_eq!(
            schema_ref_target(&root, "#/a~1b").and_then(|v| v.get("x")),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            schema_ref_target(&root, "#/c~0d").and_then(|v| v.get("y")),
            Some(&serde_json::json!(2))
        );
    }

    #[test]
    fn coverage_gaps_detect_blocking_counts() {
        let clean = SchemaCoverageBlockingFacts {
            verified_scope_json_commands_without_schema: 0,
            verified_scope_mutating_commands_without_schema: 0,
            verified_scope_json_commands_with_accepted_opaque_schema: 0,
            verified_scope_mutating_commands_with_accepted_opaque_schema: 0,
            unaccepted_opaque_schema_verbs_total: 0,
            undocumented_schema_verbs_total: 0,
        };
        assert!(coverage_has_no_blocking_schema_gaps(clean));
        let mut dirty = clean;
        dirty.undocumented_schema_verbs_total = 1;
        assert!(!coverage_has_no_blocking_schema_gaps(dirty));
    }

    #[test]
    fn coverage_field_drifts_compare_embedded_objects() {
        let sample = serde_json::json!({
            "verification": {
                "machine_contract_coverage": {
                    "summary": "old",
                    "json_commands_total": 1,
                    "accepted_opaque_schema_examples": ["transaction begin"]
                }
            },
            "command_contract_schema_coverage": {
                "summary": "old doctor",
                "json_commands_with_schema": 10
            }
        });
        let machine = serde_json::json!({
            "summary": "new",
            "json_commands_total": 2,
            "accepted_opaque_schema_examples": ["transaction begin", "transaction abort"]
        });
        let command = serde_json::json!({
            "summary": "new doctor",
            "json_commands_with_schema": 11
        });
        let issues = collect_coverage_field_drifts(&sample, &machine, &command);
        let fields: Vec<&str> = issues.iter().map(|i| i.field_path.as_str()).collect();
        assert!(fields.contains(&"verification.machine_contract_coverage.summary"));
        assert!(fields.contains(&"verification.machine_contract_coverage.json_commands_total"));
        assert!(
            fields.contains(
                &"verification.machine_contract_coverage.accepted_opaque_schema_examples"
            )
        );
        assert!(fields.contains(&"command_contract_schema_coverage.summary"));
        assert!(fields.contains(&"command_contract_schema_coverage.json_commands_with_schema"));
    }

    #[test]
    fn doctor_schemas_json_sample_span_finds_fence_body() {
        let doc = r#"
## `heddle doctor schemas --output json`

```json
{"ok": true}
```
"#;
        let (start, end) = doctor_schemas_json_sample_span(doc).expect("span");
        assert_eq!(&doc[start..end], "{\"ok\": true}");
    }

    #[test]
    fn documented_samples_bind_via_heading() {
        let doc = "\
## `heddle status --output json`

```json
{\"output_kind\": \"status\"}
```
";
        let bound = documented_samples_with_bound_verbs(doc, &["status", "log"]);
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].1, vec!["status".to_string()]);
    }

    #[test]
    fn parse_inline_verb_requires_emit_keyword() {
        assert_eq!(
            parse_inline_verb("`heddle status` emits:"),
            Some("status".into())
        );
        assert_eq!(parse_inline_verb("`heddle status` in prose"), None);
        assert_eq!(parse_inline_verb("`StatusReport` emits:"), None);
    }
}
