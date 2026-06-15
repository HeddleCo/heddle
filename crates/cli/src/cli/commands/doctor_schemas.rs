// SPDX-License-Identifier: Apache-2.0
//! `heddle doctor schemas` — drift-check `docs/json-schemas.md`
//! against schema verbs registered by the command contract table.
//!
//! Strategy
//! --------
//! 1. For every schema verb documented by the command contract table,
//!    generate the canonical schema.
//! 2. Parse `docs/json-schemas.md` and extract the literal JSON
//!    sample(s) under each `## heddle <verb> --output json` section.
//! 3. For each extracted sample, compare its top-level keys against
//!    the schema's `properties` keys. Report any sample key that
//!    isn't a property in the schema (the most common drift —
//!    field renames, deletions, typos).
//!
//! We deliberately do not pull in a full JSON-Schema validator here:
//! disk is tight, and the keys-only check catches every drift class
//! the doc has historically suffered (renames, deletions, leaks of
//! fields like `git_overlay_import_hint` into per-command outputs).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use sley::Repository as SleyRepository;

use super::{
    advice::RecoveryAdvice,
    command_catalog::{ActionTemplate, recommended_action_template},
    git_overlay_health::{MachineContractCoverage, machine_contract_coverage},
    schemas::{documented_schema_verbs, schema_for_verb, schema_verbs},
};
use crate::cli::{Cli, DoctorSchemasArgs, should_output_json};

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
    /// Stable output discriminator for JSON callers.
    pub output_kind: &'static str,
    /// One-glance machine status for this doctor surface.
    pub status: String,
    /// True only when docs, generated schemas, and catalog coverage agree.
    #[serde(rename = "verified")]
    pub verified: bool,
    /// Human-readable summary of the schema/doc contract.
    pub summary: String,
    /// Primary command to rerun or inspect when the report is not verified.
    pub recommended_action: Option<String>,
    /// Canonical fillable template for `recommended_action`; `null` when
    /// verified (no action). The always-null `_argv` sidecar was dropped
    /// (HeddleCo/heddle#254).
    pub recommended_action_template: Option<ActionTemplate>,
    /// Recovery/inspection commands in priority order.
    pub recovery_commands: Vec<String>,
    /// All verbs the runtime schema registry exposes.
    pub registered_verbs: Vec<String>,
    /// Runtime schema verbs selected by the command contract table
    /// for sample validation in `docs/json-schemas.md`.
    pub documented_verbs: Vec<String>,
    /// Runtime schema verbs that have generated schemas but are not
    /// yet selected for docs sample validation. These are coverage
    /// gaps, not drift failures.
    pub undocumented_verbs: Vec<String>,
    /// Documented verbs the doc doesn't have a `## heddle <verb>
    /// --output json` section for. (Some sections intentionally bind several
    /// verbs to one sample via inline hints; those are still matched.)
    pub unmatched_verbs: Vec<String>,
    /// Verbs the doc has a sample for, with all keys validating.
    pub passing_verbs: Vec<String>,
    /// Drift findings.
    pub issues: Vec<SchemaIssue>,
    /// Catalog-wide schema coverage for every JSON-capable command.
    /// This is broader than `registered_verbs`: registered/documented
    /// verbs prove schema drift, while this field shows the remaining
    /// command catalog gap without making drift checks ambiguous.
    pub command_contract_schema_coverage: CommandContractSchemaCoverage,
    /// Path to the doc that was checked.
    pub doc_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandContractSchemaCoverage {
    pub status: String,
    #[serde(rename = "verified_scope")]
    pub verified_scope: String,
    pub advanced_scope: String,
    pub summary: String,
    pub catalog_commands_total: usize,
    pub catalog_mutating_commands_total: usize,
    pub json_commands_total: usize,
    pub json_mutating_commands_total: usize,
    pub json_commands_with_schema: usize,
    pub json_commands_with_accepted_opaque_schema: usize,
    pub json_commands_without_schema: usize,
    #[serde(rename = "verified_scope_json_commands_total")]
    pub verified_scope_json_commands_total: usize,
    #[serde(rename = "verified_scope_json_commands_with_schema")]
    pub verified_scope_json_commands_with_schema: usize,
    #[serde(rename = "verified_scope_json_commands_with_accepted_opaque_schema")]
    pub verified_scope_json_commands_with_accepted_opaque_schema: usize,
    #[serde(rename = "verified_scope_json_commands_without_schema")]
    pub verified_scope_json_commands_without_schema: usize,
    pub advanced_scope_json_commands_total: usize,
    pub advanced_scope_json_commands_with_accepted_opaque_schema: usize,
    pub mutating_commands_total: usize,
    pub mutating_commands_with_schema: usize,
    pub mutating_commands_with_accepted_opaque_schema: usize,
    pub mutating_commands_without_schema: usize,
    #[serde(rename = "verified_scope_mutating_commands_total")]
    pub verified_scope_mutating_commands_total: usize,
    #[serde(rename = "verified_scope_mutating_commands_with_schema")]
    pub verified_scope_mutating_commands_with_schema: usize,
    #[serde(rename = "verified_scope_mutating_commands_with_accepted_opaque_schema")]
    pub verified_scope_mutating_commands_with_accepted_opaque_schema: usize,
    #[serde(rename = "verified_scope_mutating_commands_without_schema")]
    pub verified_scope_mutating_commands_without_schema: usize,
    pub advanced_scope_mutating_commands_total: usize,
    pub advanced_scope_mutating_commands_with_accepted_opaque_schema: usize,
    pub undocumented_schema_verbs_total: usize,
    pub opaque_schema_verbs_total: usize,
    pub accepted_opaque_schema_verbs_total: usize,
    pub unaccepted_opaque_schema_verbs_total: usize,
    pub missing_schema_examples: Vec<String>,
    pub missing_mutating_schema_examples: Vec<String>,
    #[serde(rename = "verified_scope_missing_schema_examples")]
    pub verified_scope_missing_schema_examples: Vec<String>,
    #[serde(rename = "verified_scope_accepted_opaque_schema_examples")]
    pub verified_scope_accepted_opaque_schema_examples: Vec<String>,
    pub advanced_scope_accepted_opaque_schema_examples: Vec<String>,
    pub accepted_opaque_schema_examples: Vec<String>,
    pub unaccepted_opaque_schema_examples: Vec<String>,
    pub undocumented_schema_examples: Vec<String>,
}

impl From<MachineContractCoverage> for CommandContractSchemaCoverage {
    fn from(coverage: MachineContractCoverage) -> Self {
        Self {
            status: coverage.status,
            verified_scope: coverage.verified_scope,
            advanced_scope: coverage.advanced_scope,
            summary: coverage.summary,
            catalog_commands_total: coverage.catalog_commands_total,
            catalog_mutating_commands_total: coverage.catalog_mutating_commands_total,
            json_commands_total: coverage.json_commands_total,
            json_mutating_commands_total: coverage.json_mutating_commands_total,
            json_commands_with_schema: coverage.json_commands_with_schema,
            json_commands_with_accepted_opaque_schema: coverage
                .json_commands_with_accepted_opaque_schema,
            json_commands_without_schema: coverage.json_commands_without_schema,
            verified_scope_json_commands_total: coverage.verified_scope_json_commands_total,
            verified_scope_json_commands_with_schema: coverage
                .verified_scope_json_commands_with_schema,
            verified_scope_json_commands_with_accepted_opaque_schema: coverage
                .verified_scope_json_commands_with_accepted_opaque_schema,
            verified_scope_json_commands_without_schema: coverage
                .verified_scope_json_commands_without_schema,
            advanced_scope_json_commands_total: coverage.advanced_scope_json_commands_total,
            advanced_scope_json_commands_with_accepted_opaque_schema: coverage
                .advanced_scope_json_commands_with_accepted_opaque_schema,
            mutating_commands_total: coverage.mutating_commands_total,
            mutating_commands_with_schema: coverage.mutating_commands_with_schema,
            mutating_commands_with_accepted_opaque_schema: coverage
                .mutating_commands_with_accepted_opaque_schema,
            mutating_commands_without_schema: coverage.mutating_commands_without_schema,
            verified_scope_mutating_commands_total: coverage.verified_scope_mutating_commands_total,
            verified_scope_mutating_commands_with_schema: coverage
                .verified_scope_mutating_commands_with_schema,
            verified_scope_mutating_commands_with_accepted_opaque_schema: coverage
                .verified_scope_mutating_commands_with_accepted_opaque_schema,
            verified_scope_mutating_commands_without_schema: coverage
                .verified_scope_mutating_commands_without_schema,
            advanced_scope_mutating_commands_total: coverage.advanced_scope_mutating_commands_total,
            advanced_scope_mutating_commands_with_accepted_opaque_schema: coverage
                .advanced_scope_mutating_commands_with_accepted_opaque_schema,
            undocumented_schema_verbs_total: coverage.undocumented_schema_verbs_total,
            opaque_schema_verbs_total: coverage.opaque_schema_verbs_total,
            accepted_opaque_schema_verbs_total: coverage.accepted_opaque_schema_verbs_total,
            unaccepted_opaque_schema_verbs_total: coverage.unaccepted_opaque_schema_verbs_total,
            missing_schema_examples: coverage.missing_schema_examples,
            missing_mutating_schema_examples: coverage.missing_mutating_schema_examples,
            verified_scope_missing_schema_examples: coverage.verified_scope_missing_schema_examples,
            verified_scope_accepted_opaque_schema_examples: coverage
                .verified_scope_accepted_opaque_schema_examples,
            advanced_scope_accepted_opaque_schema_examples: coverage
                .advanced_scope_accepted_opaque_schema_examples,
            accepted_opaque_schema_examples: coverage.accepted_opaque_schema_examples,
            unaccepted_opaque_schema_examples: coverage.unaccepted_opaque_schema_examples,
            undocumented_schema_examples: coverage.undocumented_schema_examples,
        }
    }
}

/// Public entrypoint for `heddle doctor schemas`.
pub fn cmd_doctor_schemas(cli: &Cli, args: DoctorSchemasArgs) -> Result<()> {
    let json = should_output_json(cli, None);
    let repo_root = cli.repo.clone().map(Ok).unwrap_or_else(|| {
        std::env::current_dir().map(|cwd| find_repo_root(&cwd).unwrap_or(cwd))
    })?;

    let doc_path = repo_root.join("docs").join("json-schemas.md");
    if !doc_path.exists() {
        return Err(anyhow!(doctor_schemas_doc_missing_advice(&repo_root)));
    }
    let mut doc = std::fs::read_to_string(&doc_path)
        .with_context(|| format!("read {}", doc_path.display()))?;

    if args.update_docs {
        let coverage = current_command_contract_schema_coverage();
        let updated = refresh_command_contract_coverage_sample(&doc, &coverage)
            .with_context(|| format!("update {}", doc_path.display()))?;
        if updated != doc {
            std::fs::write(&doc_path, updated.as_bytes())
                .with_context(|| format!("write {}", doc_path.display()))?;
            doc = updated;
            if !json {
                println!(
                    "Updated command-contract schema coverage sample in {}",
                    doc_path.display()
                );
                println!();
            }
        } else if !json {
            println!(
                "Command-contract schema coverage sample is already current in {}",
                doc_path.display()
            );
            println!();
        }
    }

    let report = build_schema_report(&doc_path, &doc)?;
    validate_report(&report)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_human(&report);
    }

    Ok(())
}

fn build_schema_report(doc_path: &Path, doc: &str) -> Result<SchemaReport> {
    let samples = extract_samples(&doc);

    let mut issues = Vec::new();
    let mut passing_verbs = Vec::new();
    let mut unmatched_verbs = Vec::new();

    let registered_verbs: Vec<String> = schema_verbs().iter().map(|s| s.to_string()).collect();
    let documented_verbs: Vec<String> = documented_schema_verbs()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let undocumented_verbs: Vec<String> = schema_verbs()
        .iter()
        .filter(|verb| !documented_schema_verbs().contains(verb))
        .map(|s| s.to_string())
        .collect();

    let machine_coverage = machine_contract_coverage();
    let machine_coverage_value = serde_json::to_value(&machine_coverage)
        .unwrap_or_else(|_| Value::Object(Default::default()));
    let coverage: CommandContractSchemaCoverage = machine_coverage.into();
    let command_coverage_value =
        serde_json::to_value(&coverage).unwrap_or_else(|_| Value::Object(Default::default()));

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
        let allows_additional_properties = schema_allows_additional_properties(&schema);

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
            let coverage_issue_count = issues.len();
            collect_coverage_drift_issues(
                sample,
                verb,
                &machine_coverage_value,
                &command_coverage_value,
                &mut issues,
            );
            if issues.len() != coverage_issue_count {
                verb_clean = false;
            }
            let sample_keys = match top_level_keys(&sample.json) {
                Some(keys) => keys,
                None => {
                    // Sample is the literal `null` (e.g. `review
                    // next` empty case) or a non-object value;
                    // nothing to compare key-wise.
                    continue;
                }
            };
            if allows_additional_properties {
                continue;
            }
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

    let verified = coverage_has_no_blocking_schema_gaps(&coverage)
        && unmatched_verbs.is_empty()
        && issues.is_empty();
    let status = if verified {
        coverage.status.clone()
    } else if !issues.is_empty() {
        "drift".to_string()
    } else if !unmatched_verbs.is_empty() {
        "missing_samples".to_string()
    } else {
        coverage.status.clone()
    };
    let summary = if verified {
        coverage.summary.clone()
    } else if !issues.is_empty() {
        format!("{} schema drift issue(s) found", issues.len())
    } else if !unmatched_verbs.is_empty() {
        format!(
            "{} documented schema verb(s) lack parseable samples",
            unmatched_verbs.len()
        )
    } else {
        coverage.summary.clone()
    };
    let recommended_action = (!verified).then(|| "heddle doctor schemas --output json".to_string());
    let recommended_action_template = recommended_action
        .as_deref()
        .and_then(recommended_action_template);
    let recovery_commands = if verified {
        Vec::new()
    } else {
        vec!["heddle doctor schemas --output json".to_string()]
    };

    Ok(SchemaReport {
        output_kind: "doctor_schemas",
        status,
        verified,
        summary,
        recommended_action,
        recommended_action_template,
        recovery_commands,
        registered_verbs,
        documented_verbs,
        undocumented_verbs,
        unmatched_verbs,
        passing_verbs,
        issues,
        command_contract_schema_coverage: coverage,
        doc_path: doc_path.display().to_string(),
    })
}

fn current_command_contract_schema_coverage() -> CommandContractSchemaCoverage {
    machine_contract_coverage().into()
}

fn refresh_command_contract_coverage_sample(
    doc: &str,
    coverage: &CommandContractSchemaCoverage,
) -> Result<String> {
    let (start, end) = doctor_schemas_json_sample_span(doc)?;
    let sample_text = &doc[start..end];
    let mut sample: Value =
        serde_json::from_str(sample_text).context("parse doctor schemas JSON sample")?;
    let Some(object) = sample.as_object_mut() else {
        return Err(anyhow!(
            "`heddle doctor schemas --output json` sample must be a JSON object"
        ));
    };

    object.insert(
        "summary".to_string(),
        Value::String(coverage.summary.clone()),
    );
    object.insert(
        "command_contract_schema_coverage".to_string(),
        serde_json::to_value(coverage).context("serialize command-contract schema coverage")?,
    );

    let rendered =
        serde_json::to_string_pretty(&sample).context("render doctor schemas JSON sample")?;
    let mut updated = String::with_capacity(doc.len() + rendered.len());
    updated.push_str(&doc[..start]);
    updated.push_str(&rendered);
    updated.push_str(&doc[end..]);
    Ok(updated)
}

fn doctor_schemas_json_sample_span(doc: &str) -> Result<(usize, usize)> {
    let heading = "## `heddle doctor schemas --output json`";
    let heading_start = doc
        .find(heading)
        .ok_or_else(|| anyhow!("missing `{heading}` section"))?;
    let after_heading = &doc[heading_start..];
    let fence_rel = after_heading
        .find("```json")
        .ok_or_else(|| anyhow!("missing JSON fence under `{heading}`"))?;
    let fence_start = heading_start + fence_rel;
    let content_start = doc[fence_start..]
        .find('\n')
        .map(|newline| fence_start + newline + 1)
        .ok_or_else(|| anyhow!("unterminated JSON fence under `{heading}`"))?;
    let content_end = doc[content_start..]
        .find("\n```")
        .map(|closing| content_start + closing)
        .ok_or_else(|| anyhow!("unterminated JSON fence under `{heading}`"))?;
    Ok((content_start, content_end))
}

fn validate_report(report: &SchemaReport) -> Result<()> {
    if !coverage_has_no_blocking_schema_gaps(&report.command_contract_schema_coverage) {
        return Err(anyhow!(schema_contract_advice(report)));
    }
    if !report.unmatched_verbs.is_empty() {
        return Err(anyhow!(schema_contract_advice(report)));
    }
    if !report.issues.is_empty() {
        return Err(anyhow!(schema_contract_advice(report)));
    }
    Ok(())
}

fn coverage_has_no_blocking_schema_gaps(coverage: &CommandContractSchemaCoverage) -> bool {
    coverage.verified_scope_json_commands_without_schema == 0
        && coverage.verified_scope_mutating_commands_without_schema == 0
        && coverage.verified_scope_json_commands_with_accepted_opaque_schema == 0
        && coverage.verified_scope_mutating_commands_with_accepted_opaque_schema == 0
        && coverage.unaccepted_opaque_schema_verbs_total == 0
        && coverage.undocumented_schema_verbs_total == 0
}

fn collect_coverage_drift_issues(
    sample: &DocSample,
    verb: &str,
    machine_coverage: &Value,
    command_coverage: &Value,
    issues: &mut Vec<SchemaIssue>,
) {
    collect_coverage_drift_issues_at_path(
        &sample.json,
        "",
        sample.start_line,
        verb,
        machine_coverage,
        command_coverage,
        issues,
    );
}

fn collect_coverage_drift_issues_at_path(
    value: &Value,
    path: &str,
    line: usize,
    verb: &str,
    machine_coverage: &Value,
    command_coverage: &Value,
    issues: &mut Vec<SchemaIssue>,
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
                compare_documented_coverage(
                    &child_path,
                    child,
                    machine_coverage,
                    line,
                    verb,
                    issues,
                );
            }
            "command_contract_schema_coverage" => {
                compare_documented_coverage(
                    &child_path,
                    child,
                    command_coverage,
                    line,
                    verb,
                    issues,
                );
            }
            _ => {}
        }
        collect_coverage_drift_issues_at_path(
            child,
            &child_path,
            line,
            verb,
            machine_coverage,
            command_coverage,
            issues,
        );
    }
}

fn compare_documented_coverage(
    path: &str,
    documented: &Value,
    runtime: &Value,
    line: usize,
    verb: &str,
    issues: &mut Vec<SchemaIssue>,
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
            issues.push(SchemaIssue {
                verb: verb.to_string(),
                line,
                unknown_key: field_path.clone(),
                detail: format!(
                    "sample field '{field_path}' is {}, but runtime reports {}",
                    documented_value, runtime_value
                ),
            });
        }
    }
}

fn doctor_schemas_doc_missing_advice(repo_root: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "doctor_schemas_source_docs_missing",
        "Cannot run schema docs drift check outside a Heddle source checkout",
        "Run this from the Heddle source checkout, pass `--repo <source-root>`, or use `heddle help --output json` and `heddle schemas status` for installed CLI introspection.",
        format!(
            "docs/json-schemas.md was not found under {}",
            repo_root.display()
        ),
        "`doctor schemas` compares runtime schemas to source documentation and cannot prove docs drift without that markdown file",
        "no repository objects, refs, metadata, or worktree files were changed",
        "heddle help --output json",
        vec![
            "heddle help --output json".to_string(),
            "heddle schemas status".to_string(),
            "heddle doctor schemas --output json".to_string(),
        ],
    )
}

fn schema_contract_advice(report: &SchemaReport) -> RecoveryAdvice {
    let coverage = &report.command_contract_schema_coverage;
    let error = if !coverage_has_no_blocking_schema_gaps(coverage) {
        format!(
            "Machine contract coverage is incomplete: {}",
            coverage.summary
        )
    } else if !report.unmatched_verbs.is_empty() {
        format!(
            "{} documented schema verb(s) lack parseable samples",
            report.unmatched_verbs.len()
        )
    } else if !report.issues.is_empty() {
        format!("{} schema drift issue(s) found", report.issues.len())
    } else {
        "Machine contract check failed".to_string()
    };
    let unsafe_condition = if !coverage_has_no_blocking_schema_gaps(coverage) {
        format!(
            "command catalog status `{}`; verified scope `{}` has {} JSON-capable command(s), {} mutating command(s), {} opaque schema-backed JSON command(s), {} unaccepted opaque schema verb(s), and {} runtime schema verb(s) outside the documented schema contract",
            coverage.status,
            coverage.verified_scope,
            coverage.verified_scope_json_commands_without_schema,
            coverage.verified_scope_mutating_commands_without_schema,
            coverage.verified_scope_json_commands_with_accepted_opaque_schema,
            coverage.unaccepted_opaque_schema_verbs_total,
            coverage.undocumented_schema_verbs_total
        )
    } else if !report.unmatched_verbs.is_empty() {
        format!(
            "documented schema verbs without parseable samples: {}",
            report.unmatched_verbs.join(", ")
        )
    } else {
        let examples = report
            .issues
            .iter()
            .take(3)
            .map(|issue| {
                format!(
                    "{} line {} unknown `{}`",
                    issue.verb, issue.line, issue.unknown_key
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("documented JSON samples drifted from generated schemas: {examples}")
    };
    RecoveryAdvice::machine_contract_drift(error, unsafe_condition)
}

fn render_human(report: &SchemaReport) {
    println!(
        "heddle doctor schemas — {} runtime schema verb(s), {} documented, doc: {}",
        report.registered_verbs.len(),
        report.documented_verbs.len(),
        report.doc_path
    );
    println!();
    println!(
        "  catalog_schema_coverage  {}: {}",
        report.command_contract_schema_coverage.status,
        report.command_contract_schema_coverage.summary
    );
    for verb in &report.passing_verbs {
        println!("  ok   {verb}: sample matches generated schema");
    }
    if !report.unmatched_verbs.is_empty() {
        println!();
        println!(
            "  -- {} documented verb(s) without a parseable sample:",
            report.unmatched_verbs.len()
        );
        for verb in &report.unmatched_verbs {
            println!("       {verb}");
        }
    }
    if !report.undocumented_verbs.is_empty() {
        println!();
        println!(
            "  coverage_gap  {} runtime schema verb(s) not yet sample-checked by docs:",
            report.undocumented_verbs.len()
        );
        for verb in &report.undocumented_verbs {
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
        println!("No registered-schema/documented-sample drift detected.");
        if report
            .command_contract_schema_coverage
            .json_commands_without_schema
            > 0
        {
            println!(
                "Catalog schema coverage is partial; {} JSON-capable command(s) still need registered schemas.",
                report
                    .command_contract_schema_coverage
                    .json_commands_without_schema
            );
        }
        if report
            .command_contract_schema_coverage
            .accepted_opaque_schema_verbs_total
            > 0
        {
            println!(
                "Verified everyday/agent scope is fully concrete; advanced/internal/admin scope carries {} intentionally object-shaped opaque schema verb(s).",
                report
                    .command_contract_schema_coverage
                    .accepted_opaque_schema_verbs_total
            );
        }
    }
}

/// Top-level property keys declared in a generated schema. Returns an
/// empty set when `properties` is missing (e.g. the schema is a `null`
/// or a primitive — never the case for the registry today, but handle
/// it).
fn schema_property_keys(schema: &Value) -> std::collections::BTreeSet<String> {
    schema_property_keys_from(schema, schema)
}

fn schema_property_keys_from(root: &Value, schema: &Value) -> std::collections::BTreeSet<String> {
    let mut keys: std::collections::BTreeSet<String> = schema
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

fn schema_ref_target<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for part in path.split('/') {
        let decoded = part.replace("~1", "/").replace("~0", "~");
        current = current.get(&decoded)?;
    }
    Some(current)
}

fn schema_allows_additional_properties(schema: &Value) -> bool {
    schema
        .get("additionalProperties")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// A literal JSON sample lifted out of `docs/json-schemas.md`.
#[derive(Debug)]
struct DocSample {
    /// The closest preceding `## ` heading, used as the fallback
    /// verb when no inline `heddle <verb>` reference is present.
    heading: String,
    /// Inline verb reference parsed from the most recent paragraph
    /// before the fence (e.g. `` `heddle thread marker create|delete|show`
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
            // `` `heddle thread marker create|delete|show` emit: ``.
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

/// Every `docs/json-schemas.md` ```json sample that binds to at least one
/// verb in `verbs`, paired with the subset of `verbs` it binds to (via
/// section heading or inline `heddle <verb>` hint), in document order.
///
/// This is the same heading/inline binding `heddle doctor schemas`
/// validates samples with, exposed so the `output_kind` discriminator
/// invariant can assert — sample-by-sample — that every documented sample
/// for a catalog-advertised discriminator verb carries the right
/// discriminator. Returning the full bound-verb set (not one verb at a
/// time) lets the invariant accept a grouped sample, e.g. the single
/// `heddle undo|undo --redo` sample binds to both undo modes and may
/// legitimately show either variant's discriminator. Sharing the binding
/// keeps the invariant and the production drift gate agreeing on which
/// sample documents which verb.
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

/// Bind a sample to a verb using inline hints first, then heading
/// fallback.
fn sample_matches_verb_with_hints(sample: &DocSample, verb: &str) -> bool {
    if let Some(inline) = &sample.inline_verb {
        if inline_verb_matches(inline, verb) {
            return true;
        }
        // When an inline hint is present, do *not* fall back to the
        // section heading — the inline hint is more specific and
        // overrides. Otherwise the same `thread marker create|delete|show`
        // sample would be claimed by both `thread marker list` (heading) and
        // `thread marker create` (inline), which is exactly the bug.
        return false;
    }
    sample_matches_verb(&sample.heading, verb)
}

/// Match a `verb` against an inline reference like `marker
/// create|delete|show` (the pipe-separated form is canonical in this
/// doc). Each pipe-delimited variant is compared verb-equal.
fn inline_verb_matches(inline: &str, verb: &str) -> bool {
    // Normalize: an inline hint of "thread marker create|delete|show" with
    // verb "thread marker create" should match. The hint is a sub-verb
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
    // Strip output-mode selectors so the registry-verb comparison
    // doesn't have to know about the transport flag.
    let inner_verb = strip_json_mode_tokens(inner_verb);
    // Reject obviously-non-verb backtick contents (e.g. type names,
    // path fragments). Verbs in this doc are always lowercase
    // alphanumeric tokens, optionally with `--flag` and `|`-separated
    // sub-variants.
    if !is_plausible_verb_phrase(&inner_verb) {
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

/// `## heddle status --output json` matches the verb `"status"`. Strips
/// the `heddle ` prefix, output-mode selectors, and any `<state>`-style
/// placeholders. Pipe-separated headings (e.g. `heddle bridge git
/// init|export|import|sync|push|pull --output json`) match every variant.
fn sample_matches_verb(heading: &str, verb: &str) -> bool {
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

fn strip_json_mode_tokens(input: &str) -> String {
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
    let repo = SleyRepository::discover(start).ok()?;
    repo.workdir()
        .or_else(|| repo.git_dir().parent().map(Path::to_path_buf))
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
            "`heddle bridge git status --output json`",
            "bridge git status"
        ));
        assert!(sample_matches_verb(
            "`heddle show <state> --output json`",
            "show"
        ));
        assert!(!sample_matches_verb("`heddle status --output json`", "log"));
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
    fn coverage_samples_must_match_runtime_coverage_values() {
        let sample = DocSample {
            heading: "`heddle status --output json`".to_string(),
            inline_verb: None,
            start_line: 42,
            json: serde_json::json!({
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
            }),
        };
        let machine = serde_json::json!({
            "summary": "new",
            "json_commands_total": 2,
            "accepted_opaque_schema_examples": ["transaction begin", "transaction abort"]
        });
        let command = serde_json::json!({
            "summary": "new doctor",
            "json_commands_with_schema": 11
        });
        let mut issues = Vec::new();

        collect_coverage_drift_issues(&sample, "status", &machine, &command, &mut issues);

        let fields = issues
            .iter()
            .map(|issue| issue.unknown_key.as_str())
            .collect::<Vec<_>>();
        assert!(fields.contains(&"verification.machine_contract_coverage.summary"));
        assert!(fields.contains(&"verification.machine_contract_coverage.json_commands_total"));
        assert!(
            fields.contains(
                &"verification.machine_contract_coverage.accepted_opaque_schema_examples"
            )
        );
        assert!(fields.contains(&"command_contract_schema_coverage.summary"));
        assert!(fields.contains(&"command_contract_schema_coverage.json_commands_with_schema"));
        assert!(issues.iter().all(|issue| issue.line == 42));
    }

    #[test]
    fn update_docs_refreshes_command_contract_coverage_sample() {
        let coverage = current_command_contract_schema_coverage();
        let doc = r#"
## `heddle doctor schemas --output json`

```json
{
  "output_kind": "doctor_schemas",
  "status": "available",
  "verified": true,
  "summary": "stale",
  "recommended_action": null,
  "recovery_commands": [],
  "registered_verbs": ["status"],
  "documented_verbs": ["status"],
  "undocumented_verbs": [],
  "unmatched_verbs": [],
  "passing_verbs": ["status"],
  "issues": [],
  "command_contract_schema_coverage": {
    "summary": "stale",
    "catalog_commands_total": 0
  },
  "doc_path": "/repo/docs/json-schemas.md"
}
```
"#;

        let updated =
            refresh_command_contract_coverage_sample(doc, &coverage).expect("refresh sample");
        let samples = extract_samples(&updated);
        let sample = samples
            .iter()
            .find(|sample| sample.heading == "`heddle doctor schemas --output json`")
            .expect("doctor schemas sample should still parse");

        assert_eq!(sample.json["summary"], serde_json::json!(coverage.summary));
        assert_eq!(
            sample.json["command_contract_schema_coverage"],
            serde_json::to_value(&coverage).unwrap()
        );
    }

    #[test]
    fn validate_report_returns_typed_recovery_advice_for_schema_failure() {
        let report = SchemaReport {
            output_kind: "doctor_schemas",
            status: "missing_samples".to_string(),
            verified: false,
            summary: "1 documented schema verb lacks parseable samples".to_string(),
            recommended_action: Some("heddle doctor schemas --output json".to_string()),
            recommended_action_template: recommended_action_template(
                "heddle doctor schemas --output json",
            ),
            recovery_commands: vec!["heddle doctor schemas --output json".to_string()],
            registered_verbs: vec!["status".to_string()],
            documented_verbs: vec!["status".to_string()],
            undocumented_verbs: Vec::new(),
            unmatched_verbs: vec!["status".to_string()],
            passing_verbs: Vec::new(),
            issues: Vec::new(),
            command_contract_schema_coverage: CommandContractSchemaCoverage {
                status: "available".to_string(),
                verified_scope: "everyday_and_agent".to_string(),
                advanced_scope: "advanced_internal_admin".to_string(),
                summary: "all JSON-capable commands have schemas".to_string(),
                catalog_commands_total: 1,
                catalog_mutating_commands_total: 0,
                json_commands_total: 1,
                json_mutating_commands_total: 0,
                json_commands_with_schema: 1,
                json_commands_with_accepted_opaque_schema: 0,
                json_commands_without_schema: 0,
                verified_scope_json_commands_total: 1,
                verified_scope_json_commands_with_schema: 1,
                verified_scope_json_commands_with_accepted_opaque_schema: 0,
                verified_scope_json_commands_without_schema: 0,
                advanced_scope_json_commands_total: 0,
                advanced_scope_json_commands_with_accepted_opaque_schema: 0,
                mutating_commands_total: 0,
                mutating_commands_with_schema: 0,
                mutating_commands_with_accepted_opaque_schema: 0,
                mutating_commands_without_schema: 0,
                verified_scope_mutating_commands_total: 0,
                verified_scope_mutating_commands_with_schema: 0,
                verified_scope_mutating_commands_with_accepted_opaque_schema: 0,
                verified_scope_mutating_commands_without_schema: 0,
                advanced_scope_mutating_commands_total: 0,
                advanced_scope_mutating_commands_with_accepted_opaque_schema: 0,
                undocumented_schema_verbs_total: 0,
                opaque_schema_verbs_total: 0,
                accepted_opaque_schema_verbs_total: 0,
                unaccepted_opaque_schema_verbs_total: 0,
                missing_schema_examples: Vec::new(),
                missing_mutating_schema_examples: Vec::new(),
                verified_scope_missing_schema_examples: Vec::new(),
                verified_scope_accepted_opaque_schema_examples: Vec::new(),
                advanced_scope_accepted_opaque_schema_examples: Vec::new(),
                accepted_opaque_schema_examples: Vec::new(),
                unaccepted_opaque_schema_examples: Vec::new(),
                undocumented_schema_examples: Vec::new(),
            },
            doc_path: "docs/json-schemas.md".to_string(),
        };

        let err = validate_report(&report).expect_err("schema failure should be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("schema failure should use typed recovery advice");
        assert_eq!(advice.kind, "machine_contract_drift");
        assert_eq!(
            advice.primary_command,
            "heddle doctor schemas --output json"
        );
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle doctor schemas --output json".to_string()]
        );
        assert!(!advice.hint.trim().is_empty());
        assert!(!advice.unsafe_condition.trim().is_empty());
        assert!(!advice.would_change.trim().is_empty());
        assert!(!advice.preserved.trim().is_empty());
    }
}
