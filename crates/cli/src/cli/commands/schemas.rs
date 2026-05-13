// SPDX-License-Identifier: Apache-2.0
//! `heddle schemas <verb>` — runtime introspection for CLI JSON output shapes.
//!
//! This module is the single source of truth for the JSON schemas of
//! every `--json`-emitting verb that the [`docs/json-schemas.md`]
//! contract calls out by name. The schemas are defined here as
//! schemars-derived mirror structs rather than threading
//! `JsonSchema` through every output struct in the workspace
//! (`repo`, `objects`, etc.). The cost: when a real output struct
//! changes, the mirror here must change too. The benefit:
//! `heddle doctor schemas` validates the documented samples against
//! these schemas, catching the mirror drift the same way it catches
//! doc drift.
//!
//! See [`super::doctor_schemas`] for the drift checker.

use anyhow::{Result, anyhow};
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::Value;

use crate::cli::{Cli, should_output_json};

/// All verbs whose `--json` output has a schema mirror.
///
/// The order matches the order they appear in `docs/json-schemas.md`.
pub const REGISTERED_VERBS: &[&str] = &[
    "status",
    "bridge git status",
    "log",
    "log --reflog",
    "show",
    "marker list",
    "marker create",
    "marker delete",
    "marker delete --prefix",
    "marker show",
    "thread list",
    "workspace show",
    "review show",
    "review sign",
    "review next",
    "review health",
    "transaction commit",
    "bridge git init",
    "bridge git export",
    "bridge git import",
    "bridge git sync",
    "bridge git push",
    "bridge git pull",
    "diagnose",
    // Cross-cutting: every state-changing verb may emit this stderr
    // envelope on failure when JSON output is selected. The doc binds
    // this entry via an inline-verb hint above the `{"error","hint","kind"}`
    // fence in `docs/json-schemas.md`.
    "error",
];

/// Generate the schema for `verb`. Returns `None` if no schema is registered.
pub fn schema_for_verb(verb: &str) -> Option<Value> {
    let root = match verb {
        "status" => schema_for!(StatusSchema),
        "bridge git status" => schema_for!(BridgeGitStatusSchema),
        "log" => schema_for!(LogSchema),
        "log --reflog" => schema_for!(LogReflogSchema),
        "show" => schema_for!(ShowSchema),
        "marker list" => schema_for!(MarkerListSchema),
        "marker create" | "marker delete" | "marker show" => schema_for!(MarkerOpSchema),
        "marker delete --prefix" => schema_for!(MarkerBulkDeleteSchema),
        "thread list" => schema_for!(ThreadListSchema),
        "workspace show" => schema_for!(WorkspaceShowSchema),
        "review show" => schema_for!(ReviewShowSchema),
        "review sign" => schema_for!(ReviewSignSchema),
        "review next" => schema_for!(ReviewNextSchema),
        "review health" => schema_for!(ReviewHealthSchema),
        "transaction commit" => schema_for!(TransactionCommitSchema),
        "bridge git init" => schema_for!(BridgeInitSchema),
        "bridge git export" => schema_for!(BridgeExportSchema),
        "bridge git import" => schema_for!(BridgeImportSchema),
        "bridge git sync" => schema_for!(BridgeSyncSchema),
        "bridge git push" => schema_for!(BridgePushSchema),
        "bridge git pull" => schema_for!(BridgePullSchema),
        "diagnose" => schema_for!(DiagnoseSchema),
        "error" => schema_for!(ErrorEnvelopeSchema),
        _ => return None,
    };
    serde_json::to_value(&root).ok()
}

/// Public entrypoint for `heddle schemas <verb> [--json]`.
///
/// `verb` is the joined subcommand-path string ("status", "log",
/// "bridge git status", …). Lookup is a flat string match; we don't
/// try to resolve subcommand parsing here because the registry is
/// canonical anyway.
pub fn cmd_schemas(cli: &Cli, verb: &str) -> Result<()> {
    let schema = schema_for_verb(verb).ok_or_else(|| {
        anyhow!(
            "no schema registered for verb '{verb}'. Known verbs: {}",
            REGISTERED_VERBS.join(", ")
        )
    })?;

    // `heddle schemas` always emits machine-readable JSON. The
    // `--json` flag is a no-op for parity with other verbs.
    let _json = should_output_json(cli, None);
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// Mirror types
// ---------------------------------------------------------------------------
//
// Each mirror struct mirrors the JSON wire shape of a single
// `--json`-emitting verb. The struct's `serde` attributes match the
// real serializer; the `schemars` derive produces a JSON Schema we
// emit verbatim.
//
// When you add or rename a field on a real output struct, update the
// matching mirror here and the entry in `docs/json-schemas.md`. CI
// runs `heddle doctor schemas` which validates the doc samples
// against these schemas.

// ---- shared sub-types ------------------------------------------------------
//
// Variants here are referenced only through the schemars derive,
// which the dead-code lint can't see. The annotation keeps the
// surface honest without polluting downstream warnings.
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadModeSchema {
    Lightweight,
    Materialized,
    Virtualized,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStateSchema {
    Draft,
    Active,
    Ready,
    Blocked,
    Merged,
    Abandoned,
    Promoted,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadFreshnessSchema {
    Current,
    Stale,
    Unknown,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadImpactCategorySchema {
    DependencyGraph,
    BuildRuntimeConfig,
    GeneratedOutputs,
    RepoWideRefactor,
    PublicApiSurface,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinationStatusSchema {
    Clean,
    Ahead,
    Diverged,
    Blocked,
    MergeReady,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorInfoSchema {
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StateInfoSchema {
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitCheckpointInfoSchema {
    pub git_commit: String,
    pub committed_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ChangesInfoSchema {
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ParallelThreadInfoSchema {
    pub name: String,
    pub coordination_status: CoordinationStatusSchema,
    pub current_state: Option<String>,
}

/// Operation banner — kept opaque because the underlying
/// [`repo::RepositoryOperationStatus`] is a workspace type and its
/// shape is internal. `Value` here means "any JSON object or null".
type OpaqueObject = Option<Value>;

// ---- status ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusSchema {
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub thread: Option<String>,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    pub session_id: Option<String>,
    pub heddle_session_id: Option<String>,
    pub actor: Option<ActorInfoSchema>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub usage_summary: OpaqueObject,
    pub last_progress_at: Option<String>,
    pub report_flush_state: Option<String>,
    pub attach_reason: Option<String>,
    pub thread_mode: Option<ThreadModeSchema>,
    pub thread_state: Option<ThreadStateSchema>,
    pub freshness: Option<ThreadFreshnessSchema>,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    pub task: Option<String>,
    pub promotion_suggested: bool,
    pub impact_categories: Vec<ThreadImpactCategorySchema>,
    pub heavy_impact_paths: Vec<String>,
    pub changed_path_count: usize,
    pub blockers: Vec<String>,
    pub recommended_action: String,
    pub thread_health: String,
    pub coordination_status: CoordinationStatusSchema,
    pub is_isolated: bool,
    pub parallel_threads: Vec<ParallelThreadInfoSchema>,
    pub state: Option<StateInfoSchema>,
    pub git_checkpoint: Option<GitCheckpointInfoSchema>,
    pub changes: ChangesInfoSchema,
}

// ---- bridge git status ----------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeGitStatusSchema {
    pub repository_capability: String,
    pub storage_model: String,
    pub mirror_path: Option<String>,
    pub mirror_initialized: bool,
    pub git_overlay_import_hint: Option<GitOverlayImportHintSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitOverlayImportHintSchema {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

// ---- log ------------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct LogSchema {
    pub repository_capability: String,
    pub storage_model: String,
    pub states: Vec<StateEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StateEntrySchema {
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub principal: String,
    pub agent: Option<String>,
    pub confidence: Option<f32>,
    pub created_at: String,
    pub parents: Vec<String>,
    pub git_checkpoint: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LogReflogSchema {
    pub repository_capability: String,
    pub storage_model: String,
    pub entries: Vec<ReflogEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReflogEntrySchema {
    pub source: String,
    pub reference: String,
    pub old_oid: String,
    pub new_oid: String,
    pub actor: String,
    pub timestamp: Option<String>,
    pub message: String,
}

// ---- show -----------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShowSchema {
    pub repository_capability: String,
    pub storage_model: String,
    pub change_id: String,
    pub change_id_full: String,
    pub content_hash: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub principal: ShowPrincipalSchema,
    pub agent: Option<ShowAgentSchema>,
    pub created_at: String,
    pub status: String,
    pub verification: OpaqueObject,
    pub git_checkpoint: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShowPrincipalSchema {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShowAgentSchema {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub policy_id: Option<String>,
}

// ---- marker ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct MarkerListSchema {
    pub markers: Vec<MarkerEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MarkerEntrySchema {
    pub name: String,
    pub change_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MarkerOpSchema {
    pub name: String,
    pub change_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MarkerBulkDeleteSchema {
    pub deleted: Vec<MarkerEntrySchema>,
    pub count: usize,
    pub message: String,
}

// ---- thread list ----------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadListSchema {
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub threads: Vec<Value>,
    pub current: Option<String>,
}

// ---- workspace show -------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkspaceShowSchema {
    pub repository: String,
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub recommended_action: String,
    pub current_thread: Option<String>,
    pub groups: Vec<WorkspaceGroupSchema>,
    pub thread_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkspaceGroupSchema {
    pub id: String,
    pub label: String,
    pub threads: Vec<Value>,
}

// ---- review ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewShowSchema {
    pub change_id: String,
    pub headline: String,
    pub agent_narrative: Option<String>,
    pub files_changed: usize,
    pub in_budget_signals: Vec<Value>,
    pub all_signals: Vec<Value>,
    pub discussions: Vec<Value>,
    pub signing_kinds: Vec<String>,
    pub signatures: Vec<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewSignSchema {
    pub signature_id: String,
    pub change_id: String,
}

/// `heddle review next --json` emits either a populated object or the
/// literal `null`. We model the populated shape; the `null` case is
/// allowed by the doc and isn't covered here.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewNextSchema {
    pub change_id: String,
    pub headline: String,
    pub existing_signatures: Vec<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewHealthSchema {
    pub entries: Vec<ReviewHealthEntrySchema>,
    pub window_states: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewHealthEntrySchema {
    pub module_id: String,
    pub fire_rate: f64,
    pub warn: bool,
}

// ---- transaction commit ---------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct TransactionCommitSchema {
    pub change_id: String,
    pub op_count: u32,
}

// ---- bridge ops -----------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeInitSchema {
    pub initialized: bool,
    pub path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeExportSchema {
    pub states_exported: u64,
    pub threads_synced: u64,
    pub markers_synced: u64,
    pub destination: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeImportSchema {
    pub commits_imported: u64,
    pub states_created: u64,
    pub branches_synced: u64,
    pub tags_synced: u64,
    pub skipped_non_commit_refs: u64,
    pub partial_mirror_refs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeSyncSchema {
    pub states_exported: u64,
    pub commits_imported: u64,
    pub threads_synced: u64,
    pub markers_synced: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgePushSchema {
    pub pushed: bool,
    pub remote: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgePullSchema {
    pub pulled: bool,
    pub remote: String,
}

// ---- diagnose -------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiagnoseSchema {
    pub repository: String,
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub git_overlay_import_hint: Option<GitOverlayImportHintSchema>,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub thread: Option<Value>,
    pub state: Option<Value>,
    pub changes: Value,
    pub workspace: Value,
    pub health: Value,
    pub profile: Option<Value>,
}

// ---- error envelope (cross-cutting) ---------------------------------------
//
// Emitted to **stderr** (not stdout) by any state-changing verb that fails
// when JSON output is selected. The 21 verb schemas above describe the
// stdout success shape; this schema describes the stderr failure shape so
// scripts and agents can parse failures without scraping freeform text.
//
// Field contract:
//
// - `error` — human-readable message (the anyhow chain rendered via `{:#}`).
//   Always present, never empty.
// - `hint` — single-line next-step recommendation. Empty string when no
//   actionable hint applies. The CLI promises to suggest a real
//   `heddle <verb>` invocation whenever one fits.
// - `kind` — stable predicate name keying the hint family. Empty string
//   when the error didn't match a known class. Current values:
//   `repository_not_found`, `repository_exists`, `state_not_found`,
//   `thread_not_found`, `out_of_space`, `permission_denied`,
//   `read_only_filesystem`. New kinds may be added (additive); existing
//   ones are stable.

#[derive(Debug, Serialize, JsonSchema)]
pub struct ErrorEnvelopeSchema {
    pub error: String,
    pub hint: String,
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry in `REGISTERED_VERBS` must produce a schema.
    /// Otherwise `heddle doctor schemas` would silently miss drift on
    /// that verb.
    #[test]
    fn registry_covers_every_listed_verb() {
        for verb in REGISTERED_VERBS {
            assert!(
                schema_for_verb(verb).is_some(),
                "verb '{verb}' is in REGISTERED_VERBS but schema_for_verb returned None"
            );
        }
    }

    #[test]
    fn unknown_verb_returns_none() {
        assert!(schema_for_verb("nope").is_none());
    }

    #[test]
    fn status_schema_has_expected_top_level_properties() {
        let schema = schema_for_verb("status").expect("status schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("status schema has properties");
        for required in &[
            "repository_capability",
            "storage_model",
            "hosted_enabled",
            "thread",
            "current_state",
            "actor",
            "blockers",
            "changes",
        ] {
            assert!(
                properties.contains_key(*required),
                "status schema missing property '{required}'"
            );
        }
    }

    #[test]
    fn log_schema_has_states_array() {
        let schema = schema_for_verb("log").expect("log schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(properties.contains_key("states"));
        assert!(properties.contains_key("repository_capability"));
    }
}