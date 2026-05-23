// SPDX-License-Identifier: Apache-2.0
//! `heddle doctor schemas` — drift-check `docs/json-schemas.md`
//! against schema verbs registered by the command contract table.
//!
//! Strategy
//! --------
//! 1. For every schema verb documented by the command contract table,
//!    generate the canonical schema.
//! 2. Parse `docs/json-schemas.md` and extract the literal JSON
//!    sample(s) under each `## heddle <verb> --json` section.
//! 3. For each extracted sample, compare its top-level keys against
//!    the schema's `properties` keys. Report any sample key that
//!    isn't a property in the schema (the most common drift —
//!    field renames, deletions, typos).
//!
//! We deliberately do not pull in a full JSON-Schema validator here:
//! disk is tight, and the keys-only check catches every drift class
//! the doc has historically suffered (renames, deletions, leaks of
//! fields like `git_overlay_import_hint` into per-command outputs).

use std::{
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;

use super::schemas::{documented_schema_verbs, schema_for_verb};
use crate::cli::{Cli, should_output_json};

/// One drift finding.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaIssue {
    /// Verb the sample documents.
    pub verb: String,
    /// 1-based line in `docs/json-schemas.md` where the sample begins.
    pub line: usize,
    /// Sample key the schema doesn't declare.
    pub unknown_key: String,
    /// One-line human description for the text renderer.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaReport {
    /// All verbs the registry exposes.
    pub registered_verbs: Vec<String>,
    /// Verbs the doc doesn't have a `## heddle <verb> --json`
    /// section for. (Some of these are deliberately uncovered —
    /// e.g. `marker show` shares `MarkerOpSchema` with `marker
    /// create`. The renderer just lists them.)
    pub unmatched_verbs: Vec<String>,
    /// Verbs the doc has a sample for, with all keys validating.
    pub passing_verbs: Vec<String>,
    /// Drift findings.
    pub issues: Vec<SchemaIssue>,
    /// Path to the doc that was checked.
    pub doc_path: String,
}

/// Public entrypoint for `heddle doctor schemas`.
pub fn cmd_doctor_schemas(cli: &Cli) -> Result<()> {
    let json = should_output_json(cli, None);
    let repo_root = cli.repo.clone().map(Ok).unwrap_or_else(|| {
        std::env::current_dir().map(|cwd| find_repo_root(&cwd).unwrap_or(cwd))
    })?;

    let doc_path = repo_root.join("docs").join("json-schemas.md");
    let doc = std::fs::read_to_string(&doc_path)
        .with_context(|| format!("read {}", doc_path.display()))?;

    let samples = extract_samples(&doc);

    let mut issues = Vec::new();
    let mut passing_verbs = Vec::new();
    let mut unmatched_verbs = Vec::new();

    for verb in documented_schema_verbs() {
        let schema = match schema_for_verb(verb) {
            Some(s) => s,
            None => {
                // Should never happen because the unit test pins it,
                // but stay defensive.
                continue;
            }
        };
        let property_keys = schema_property_keys(&schema);

        // Look up the sample(s) for this verb. Missing samples are
        // reported as "unmatched" rather than as drift, so dropping
        // a verb from the doc shows up here without rejecting CI.
        //
        // Two paths into a sample:
        //   1. The inline verb hint (e.g. text like `heddle marker
        //      create|delete|show` immediately above the fence) —
        //      this is the precise binding when a section has
        //      multiple samples for distinct verbs.
        //   2. Fallback to the section heading.
        let verb_samples: Vec<&DocSample> = samples
            .iter()
            .filter(|s| sample_matches_verb_with_hints(s, verb))
            .collect();

        if verb_samples.is_empty() {
            unmatched_verbs.push((*verb).to_string());
            continue;
        }

        let mut verb_clean = true;
        for sample in verb_samples {
            let sample_keys = match top_level_keys(&sample.json) {
                Some(keys) => keys,
                None => {
                    // Sample is the literal `null` (e.g. `review
                    // next` empty case) or a non-object value;
                    // nothing to compare key-wise.
                    continue;
                }
            };
            for key in sample_keys {
                if !property_keys.contains(&key) {
                    verb_clean = false;
                    issues.push(SchemaIssue {
                        verb: (*verb).to_string(),
                        line: sample.start_line,
                        unknown_key: key.clone(),
                        detail: format!("sample has field '{key}', but schema does not declare it"),
                    });
                }
            }
        }
        if verb_clean {
            passing_verbs.push((*verb).to_string());
        }
    }

    let report = SchemaReport {
        registered_verbs: documented_schema_verbs()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        unmatched_verbs,
        passing_verbs,
        issues,
        doc_path: doc_path.display().to_string(),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_human(&report);
    }

    if !report.issues.is_empty() {
        return Err(anyhow!(
            "{} schema drift issue(s) found",
            report.issues.len()
        ));
    }
    Ok(())
}

fn render_human(report: &SchemaReport) {
    println!(
        "heddle doctor schemas — {} verb(s) registered, doc: {}",
        report.registered_verbs.len(),
        report.doc_path
    );
    println!();
    for verb in &report.passing_verbs {
        println!("  ok   {verb}: sample matches generated schema");
    }
    if !report.unmatched_verbs.is_empty() {
        println!();
        println!(
            "  -- {} verb(s) without a documented sample (allowed):",
            report.unmatched_verbs.len()
        );
        for verb in &report.unmatched_verbs {
            println!("       {verb}");
        }
    }
    if !report.issues.is_empty() {
        println!();
        for issue in &report.issues {
            println!(
                "  drift  {}: {} (doc line {})",
                issue.verb, issue.detail, issue.line
            );
        }
        println!();
        println!("Found {} drift issue(s).", report.issues.len());
    } else {
        println!();
        println!("No drift detected.");
    }
}

/// Top-level property keys declared in a generated schema. Returns an
/// empty set when `properties` is missing (e.g. the schema is a `null`
/// or a primitive — never the case for the registry today, but handle
/// it).
fn schema_property_keys(schema: &Value) -> std::collections::BTreeSet<String> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// A literal JSON sample lifted out of `docs/json-schemas.md`.
#[derive(Debug)]
struct DocSample {
    /// The closest preceding `## ` heading, used as the fallback
    /// verb when no inline `heddle <verb>` reference is present.
    heading: String,
    /// Inline verb reference parsed from the most recent paragraph
    /// before the fence (e.g. `` `heddle marker create|delete|show`
    /// emit: ``). When present, this overrides the section heading.
    inline_verb: Option<String>,
    /// 1-based line number where the ```json fence opens.
    start_line: usize,
    /// Parsed JSON. May be a non-object (e.g. literal `null`).
    json: Value,
}

/// Walk every fenced ```json block in the doc and pair it with both
/// the nearest preceding `## ` heading and the most recent inline
/// `heddle <verb>` reference. Skips fences whose JSON doesn't parse
/// — those are rare placeholder snippets (e.g. samples with `...`
/// fillers) that we deliberately don't validate.
fn extract_samples(doc: &str) -> Vec<DocSample> {
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
            // `` `heddle marker create|delete|show` emit: ``.
            // Also reset the hint on a blank line that isn't immediately
            // before a fence — that way only references in the
            // paragraph adjacent to the fence stick.
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

/// Bind a sample to a verb using inline hints first, then heading
/// fallback.
fn sample_matches_verb_with_hints(sample: &DocSample, verb: &str) -> bool {
    if let Some(inline) = &sample.inline_verb {
        if inline_verb_matches(inline, verb) {
            return true;
        }
        // When an inline hint is present, do *not* fall back to the
        // section heading — the inline hint is more specific and
        // overrides. Otherwise the same `marker create|delete|show`
        // sample would be claimed by both `marker list` (heading) and
        // `marker create` (inline), which is exactly the bug.
        return false;
    }
    sample_matches_verb(&sample.heading, verb)
}

/// Match a `verb` against an inline reference like `marker
/// create|delete|show` (the pipe-separated form is canonical in this
/// doc). Each pipe-delimited variant is compared verb-equal.
fn inline_verb_matches(inline: &str, verb: &str) -> bool {
    // Normalize: an inline hint of "marker create|delete|show" with
    // verb "marker create" should match. The hint is a sub-verb
    // expansion: split the LAST whitespace-separated token on `|`
    // and try each form.
    let trimmed = inline.trim();
    if trimmed == verb {
        return true;
    }
    // If the hint contains a pipe in the last token, try each form.
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
/// followed by `emits:` or `emit:`. Returns `None` for everything
/// else (including ordinary `heddle <verb>` mentions in prose that
/// don't introduce a sample).
fn parse_inline_verb(line: &str) -> Option<String> {
    // Look for the pattern: `heddle <verb>` (...) emits | emit
    let bytes = line.as_bytes();
    let backtick_start = line.find('`')?;
    // Find the closing backtick after backtick_start.
    let after_first = &line[backtick_start + 1..];
    let backtick_end_rel = after_first.find('`')?;
    let inner = &after_first[..backtick_end_rel];
    let inner = inner.trim();
    // The doc uses both `heddle <verb>` and `<verb>` forms inline.
    // Accept either — when the prefix is absent, the inner string
    // itself is treated as the verb candidate.
    let inner_verb = inner.strip_prefix("heddle ").unwrap_or(inner).trim();
    // Strip a trailing `--json` so the registry-verb comparison
    // doesn't have to know about it.
    let inner_verb = inner_verb.trim_end_matches("--json").trim();
    // Reject obviously-non-verb backtick contents (e.g. type names,
    // path fragments). Verbs in this doc are always lowercase
    // alphanumeric tokens, optionally with `--flag` and `|`-separated
    // sub-variants.
    if !is_plausible_verb_phrase(inner_verb) {
        return None;
    }
    // Confirm "emits" / "emit" appears later in the line.
    let after_close = &line[backtick_start + 1 + backtick_end_rel + 1..];
    let after_close_lower = after_close.to_ascii_lowercase();
    // Allow trailing colons, dashes, etc.
    let _ = bytes; // silence unused warning if linter complains.
    if after_close_lower.contains("emits") || after_close_lower.contains("emit") {
        Some(inner_verb.to_string())
    } else {
        None
    }
}

/// Lightweight plausibility check for an inline-verb candidate.
/// We accept lowercase ASCII letters, digits, hyphens, pipes,
/// spaces, angle brackets, and the literal `--flag` form. Anything
/// else (uppercase, dots, slashes) is rejected so that backtick-
/// fenced type names or paths don't pollute the matching.
fn is_plausible_verb_phrase(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || matches!(c, ' ' | '-' | '|' | '<' | '>' | '_')
    })
}

/// `## heddle status --json` matches the verb `"status"`. Strips
/// the `heddle ` prefix, the `--json` suffix, and any `<state>`-style
/// placeholders. Pipe-separated headings (e.g. `heddle bridge git
/// init|export|import|sync|push|pull --json`) match every variant.
fn sample_matches_verb(heading: &str, verb: &str) -> bool {
    let stripped = heading.trim_start_matches('`').trim_end_matches('`').trim();
    let stripped = stripped.trim_start_matches("heddle ").trim();
    let mut tokens: Vec<&str> = stripped
        .split_whitespace()
        .filter(|tok| !tok.starts_with('<') && *tok != "--json")
        .collect();
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

/// Returns top-level keys when `value` is an object. None for
/// primitives, arrays, and `null` — those are valid sample shapes
/// (e.g. `review next` returning literal null) but contribute no
/// keys to compare.
fn top_level_keys(value: &Value) -> Option<Vec<String>> {
    let object = value.as_object()?;
    Some(object.keys().cloned().collect())
}

/// Walk parents of `start` until we find a directory containing a
/// `.heddle/` or `.git/` folder. Same heuristic as
/// [`super::doctor_docs::find_repo_root`] but kept private to this
/// module to avoid a doc-only intermodule coupling.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        if ancestor.join(".heddle").exists() || ancestor.join(".git").exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    // Fallback: ask `git rev-parse --show-toplevel` so this works
    // inside Heddle worktrees that don't co-locate `.git/`.
    let output = ProcessCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    Some(PathBuf::from(path.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_samples_from_simple_doc() {
        let doc = "\
## `heddle foo --json`

Some prose.

```json
{\"a\": 1, \"b\": 2}
```

## `heddle bar --json`

```json
{\"x\": true}
```
";
        let samples = extract_samples(doc);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].heading, "`heddle foo --json`");
        assert_eq!(samples[0].json.get("a").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(samples[1].heading, "`heddle bar --json`");
    }

    #[test]
    fn skips_fences_with_nonparseable_placeholder_samples() {
        let doc = "\
## `heddle baz --json`

```json
{\"placeholder\": ...}
```
";
        let samples = extract_samples(doc);
        assert!(samples.is_empty());
    }

    #[test]
    fn sample_matches_verb_strips_heddle_prefix_and_args() {
        assert!(sample_matches_verb("`heddle status --json`", "status"));
        assert!(sample_matches_verb(
            "`heddle bridge git status --json`",
            "bridge git status"
        ));
        assert!(sample_matches_verb("`heddle show <state> --json`", "show"));
        assert!(!sample_matches_verb("`heddle status --json`", "log"));
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
}
