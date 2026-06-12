// SPDX-License-Identifier: Apache-2.0
//! `heddle schemas <verb>` — runtime introspection for CLI JSON output shapes.
//!
//! This module owns the JSON schema mirrors for `--output json`-emitting
//! verbs. Schema registration metadata comes from the active command
//! catalog; the schemas themselves are schemars-derived mirror structs
//! rather than threading
//! `JsonSchema` through every output struct in the workspace
//! (`repo`, `objects`, etc.). The cost: when a real output struct
//! changes, the mirror here must change too. The benefit:
//! `heddle doctor schemas` validates the documented samples against
//! these schemas, catching the mirror drift the same way it catches
//! doc drift.
//!
//! See [`super::doctor_schemas`] for the drift checker.

use std::{collections::BTreeMap, sync::OnceLock};

use anyhow::{anyhow, Result};
use schemars::{schema_for, JsonSchema};
use serde::Serialize;
use serde_json::Value;

use super::{command_catalog, command_runtime_contract, CommandCatalogOutput, RecoveryAdvice};
use crate::cli::{should_output_json, Cli};

static SCHEMA_VERBS: OnceLock<Vec<&'static str>> = OnceLock::new();
static DOCUMENTED_SCHEMA_VERBS: OnceLock<Vec<&'static str>> = OnceLock::new();
static OPAQUE_SCHEMA_VERBS: OnceLock<Vec<&'static str>> = OnceLock::new();

macro_rules! schema_registry {
    ($(($verbs:expr, $schema:ty)),+ $(,)?) => {
        fn schema_for_registered_verb(verb: &str) -> Option<Value> {
            $(
                if $verbs.contains(&verb) {
                    let root = schema_for!($schema);
                    return serde_json::to_value(&root).ok();
                }
            )+
            None
        }

        #[cfg(test)]
        fn schema_implementation_verbs() -> Vec<&'static str> {
            let mut verbs = Vec::new();
            $(
                for verb in $verbs {
                    if !verbs.contains(verb) {
                        verbs.push(*verb);
                    }
                }
            )+
            verbs
        }
    };
}

schema_registry! {
    (&["init"], InitSchema),
    (&["status"], StatusSchema),
    (&["verify"], VerifySchema),
    (&["adopt"], AdoptSchema),
    (&["attempt"], AttemptSchema),
    (&["capture"], CaptureSchema),
    (&["commit"], CommitSchema),
    (&["checkpoint"], CheckpointSchema),
    (&["undo"], UndoSchema),
    // heddle#473 phase 1: `redo` is its own top-level verb again (re-split from
    // the brief `undo --redo` fold), and `undo --list` is undo's history view.
    // Both emit their own `output_kind`, so both need a schema mirror. `redo`
    // shares `undo`'s payload (`UndoSchema`); the `--list` view has its own
    // list-shaped schema.
    (&["redo"], UndoSchema),
    (&["undo --list"], UndoListSchema),
    (&["clean"], CleanSchema),
    (&["diff"], DiffSchema),
    (&["goto"], GotoSchema),
    (&["branch"], BranchCompatSchema),
    (&["switch"], SwitchCheckoutSchema),
    (&["merge --preview"], MergePreviewSchema),
    (&["ready"], ReadySchema),
    (&["land"], LandSchema),
    (&["sync"], SyncSchema),
    (&["continue", "abort"], OperatorCommandSchema),
    (&["delegate"], DelegateSchema),
    (&["start"], ThreadStartSchema),
    (&["thread create", "thread switch", "thread rename"], ThreadStartSchema),
    (&["thread current"], ThreadCurrentSchema),
    (&["thread captures"], ThreadCapturesSchema),
    (&["thread refresh", "thread drop"], ThreadCommandSchema),
    (&["thread promote"], ThreadCommandSchema),
    (&["thread move"], ThreadMoveSchema),
    (&["thread absorb"], ThreadAbsorbSchema),
    (&["thread resolve"], ThreadResolveSchema),
    (&["thread approve"], ThreadApprovalSchema),
    (&["thread approvals"], ThreadApprovalListSchema),
    (&["thread revoke-approval"], ThreadRevokeApprovalSchema),
    (&["thread check-merge"], ThreadMergeEligibilitySchema),
    (&["thread cleanup"], ThreadCleanupSchema),
    (&["thread show"], ThreadShowSchema),
    (&["clone"], CloneSchema),
    (&["remote list"], RemoteListSchema),
    (&["remote show"], RemoteInfoSchema),
    (&["remote add", "remote remove", "remote set-default"], RemoteMutationSchema),
    (&["fetch"], FetchSchema),
    (&["pull"], PullSchema),
    (&["push"], PushSchema),
    (&["bridge git status"], BridgeGitStatusSchema),
    (&["log"], LogSchema),
    (&["log --reflog"], LogReflogSchema),
    (&["show"], ShowSchema),
    (&["marker list"], MarkerListSchema),
    (&["marker create", "marker delete", "marker show"], MarkerOpSchema),
    (&["marker delete --prefix"], MarkerBulkDeleteSchema),
    (&["thread list"], ThreadListSchema),
    (&["workspace show"], WorkspaceShowSchema),
    (&["commands"], CommandCatalogOutput),
    (&["schemas"], SchemasListSchema),
    (&["review show"], ReviewShowSchema),
    (&["review sign"], ReviewSignSchema),
    (&["review next"], ReviewNextSchema),
    (&["review health"], ReviewHealthSchema),
    (&["inspect"], InspectSchema),
    (&["retro"], RetroSchema),
    (&["discuss open", "discuss append", "discuss resolve", "discuss show"], DiscussionEnvelopeSchema),
    (&["discuss list"], DiscussionListSchema),
    (&["query"], QuerySchema),
    (&["transaction commit"], TransactionCommitSchema),
    (&["bridge git init"], BridgeInitSchema),
    (&["bridge git export"], BridgeExportSchema),
    (&["bridge git import"], BridgeImportSchema),
    (&["bridge git sync"], BridgeSyncSchema),
    (&["bridge git reconcile"], BridgeGitReconcileSchema),
    (&["bridge git push"], BridgePushSchema),
    (&["bridge git pull"], BridgePullSchema),
    (&["stash push", "stash pop", "stash apply", "stash drop", "stash clear"], StashMutationSchema),
    (&["stash list"], StashListSchema),
    (&["stash show"], StashShowSchema),
    (&["revert"], RevertSchema),
    (&["doctor"], DiagnoseSchema),
    (&["doctor docs"], DoctorDocsSchema),
    (&["doctor schemas"], DoctorSchemasSchema),
    (&["actor spawn", "actor show"], ActorSingleSchema),
    (&["actor list"], ActorListSchema),
    (&["actor done"], ActorDoneSchema),
    (&["actor explain"], ActorExplainSchema),
    (&["agent serve"], AgentServeSchema),
    (&["agent status"], AgentDaemonStatusSchema),
    (&["agent stop"], AgentStopSchema),
    (&["agent reserve", "agent heartbeat", "agent release"], AgentReservationEnvelopeSchema),
    (&["agent capture"], CaptureSchema),
    (&["agent ready"], ReadySchema),
    (&["agent list"], AgentReservationListSchema),
    (&["session start", "session end", "session show"], SessionEnvelopeSchema),
    (&["session segment"], SessionSegmentEnvelopeSchema),
    (&["session list"], SessionListSchema),
    (&["git-overlay"], GitOverlayGuideSchema),
    (&["watch"], WatchLineSchema),
    (&["try"], TrySchema),
    (&["blame"], BlameSchema),
    (&["fsck"], FsckSchema),
    (&["resolve"], ResolveSchema),
    (&["maintenance index"], IndexSchema),
    (&["error"], ErrorEnvelopeSchema),
}

/// All verbs whose `--output json` output has a schema mirror, derived from
/// the active command catalog.
pub fn schema_verbs() -> &'static [&'static str] {
    SCHEMA_VERBS
        .get_or_init(command_catalog::schema_verbs)
        .as_slice()
}

/// Schema verbs that `heddle doctor schemas` must check against
/// `docs/json-schemas.md`, derived from the active command catalog.
pub fn documented_schema_verbs() -> &'static [&'static str] {
    DOCUMENTED_SCHEMA_VERBS
        .get_or_init(command_catalog::documented_schema_verbs)
        .as_slice()
}

/// Runtime schema verbs that intentionally expose only an opaque JSON
/// object shape. Coverage reports count these separately from
/// concrete schema mirrors.
pub(crate) fn opaque_schema_verbs() -> &'static [&'static str] {
    OPAQUE_SCHEMA_VERBS
        .get_or_init(command_catalog::opaque_schema_verbs)
        .as_slice()
}

/// Generate the schema for `verb`. Returns `None` if no schema is registered.
pub fn schema_for_verb(verb: &str) -> Option<Value> {
    let verb = resolve_schema_verb(verb)?;
    if !schema_verbs().contains(&verb) {
        return None;
    }
    let mut schema = schema_for_registered_verb(verb).or_else(|| {
        opaque_schema_verbs()
            .contains(&verb)
            .then(|| serde_json::to_value(schema_for!(GenericJsonObjectSchema)).ok())
            .flatten()
    })?;
    add_op_id_replay_fields_if_supported(verb, &mut schema);
    add_json_discriminator_if_advertised(verb, &mut schema);
    Some(schema)
}

fn resolve_schema_verb(verb: &str) -> Option<&'static str> {
    let verb = verb.trim();
    if let Some(registered) = schema_verbs()
        .iter()
        .copied()
        .find(|registered| *registered == verb)
    {
        return Some(registered);
    }

    let matches = matching_schema_verbs(verb, schema_verbs());
    if matches.len() == 1 {
        matches.first().copied()
    } else {
        None
    }
}

#[cfg(test)]
const OP_ID_REPLAY_FIELD_NAMES: &[&str] = &[
    "op_id",
    "operation_record",
    "idempotency_status",
    "replayed",
];

fn add_op_id_replay_fields_if_supported(verb: &str, schema: &mut Value) {
    if !schema_verb_supports_op_id(verb) {
        return;
    }

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let properties = object
        .entry("properties".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(properties) = properties.as_object_mut() else {
        return;
    };

    properties
        .entry("op_id".to_string())
        .or_insert_with(|| serde_json::json!({ "type": ["string", "null"] }));
    properties
        .entry("idempotency_status".to_string())
        .or_insert_with(|| serde_json::json!({ "type": ["string", "null"] }));
    properties
        .entry("replayed".to_string())
        .or_insert_with(|| serde_json::json!({ "type": ["boolean", "null"] }));
    properties
        .entry("operation_record".to_string())
        .or_insert_with(|| {
            serde_json::json!({
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "op_id": { "type": "string" },
                            "command": { "type": "string" },
                            "idempotency_status": { "type": "string" },
                            "replayed": { "type": "boolean" }
                        },
                        "required": [
                            "command",
                            "idempotency_status",
                            "op_id",
                            "replayed"
                        ]
                    },
                    { "type": "null" }
                ]
            })
        });
}

fn add_json_discriminator_if_advertised(verb: &str, schema: &mut Value) {
    let Some(discriminator) = command_catalog::command_json_discriminator_for_schema_verb(verb)
    else {
        return;
    };

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let properties = object
        .entry("properties".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(properties) = properties.as_object_mut() else {
        return;
    };
    properties.insert(
        discriminator.field.clone(),
        serde_json::json!({
            "type": "string",
            "enum": [discriminator.value],
        }),
    );

    let required = object
        .entry("required".to_string())
        .or_insert_with(|| serde_json::json!([]));
    let Some(required) = required.as_array_mut() else {
        return;
    };
    if !required
        .iter()
        .any(|field| field.as_str() == Some(discriminator.field.as_str()))
    {
        required.push(Value::String(discriminator.field));
    }
}

fn schema_verb_supports_op_id(verb: &str) -> bool {
    command_runtime_contract(verb).is_some_and(|contract| contract.supports_op_id)
        || command_runtime_contract(&verb_without_flags(verb))
            .is_some_and(|contract| contract.supports_op_id)
}

fn verb_without_flags(verb: &str) -> String {
    verb.split_whitespace()
        .filter(|part| !part.starts_with('-'))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Public entrypoint for `heddle schemas [<verb>]`.
///
/// `verb_parts` are the raw trailing tokens after `schemas`.
/// Lookup is a flat string match after filtering global flags that
/// clap cannot see because the schema command intentionally accepts
/// literal command flags like `--preview` and `--reflog`.
pub fn cmd_schemas(cli: &Cli, verb_parts: &[String]) -> Result<()> {
    let verb_parts = normalize_schema_verb_parts(verb_parts)?;
    if verb_parts.is_empty() {
        let out = serde_json::json!({
            "output_kind": "schemas",
            "status": "completed",
            "schema_verbs": schema_verbs(),
            "documented_schema_verbs": documented_schema_verbs(),
        });
        return render_schema_json(&out);
    }

    let verb = verb_parts.join(" ");
    let schema = schema_for_verb(&verb)
        .ok_or_else(|| anyhow!(schema_not_registered_advice(&verb, schema_verbs())))?;

    // `heddle schemas` always emits machine-readable JSON.
    let _json = should_output_json(cli, None);
    render_schema_json(&schema)
}

fn render_schema_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn schema_not_registered_advice(verb: &str, known_verbs: &[&str]) -> RecoveryAdvice {
    let matches = suggested_schema_verbs(verb, known_verbs);
    let primary_command = matches
        .first()
        .map(|matched| format!("heddle schemas {matched}"))
        .unwrap_or_else(|| "heddle schemas".to_string());
    let hint = if matches.is_empty() {
        "Run `heddle schemas` to list schema-backed verbs, or inspect the command catalog with `heddle commands --output json`.".to_string()
    } else {
        format!(
            "`{verb}` is not exact; available schema verb{}: {}.",
            if matches.len() == 1 { "" } else { "s" },
            matches
                .iter()
                .map(|matched| format!("`{matched}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut recovery_commands = Vec::new();
    push_unique_command(&mut recovery_commands, primary_command.clone());
    push_unique_command(&mut recovery_commands, "heddle schemas".to_string());
    push_unique_command(
        &mut recovery_commands,
        "heddle commands --output json".to_string(),
    );
    for matched in matches.iter().skip(1) {
        push_unique_command(&mut recovery_commands, format!("heddle schemas {matched}"));
    }

    RecoveryAdvice::safety_refusal(
        "schema_not_registered",
        format!("No JSON schema is registered for `{verb}`"),
        hint,
        format!("`{verb}` is not in the runtime schema registry"),
        "schema lookup does not change repository state; retrying the same unknown verb will fail until a schema is registered",
        "no repository objects, refs, metadata, or worktree files were changed",
        primary_command,
        recovery_commands,
    )
}

fn suggested_schema_verbs<'a>(verb: &str, known_verbs: &'a [&'a str]) -> Vec<&'a str> {
    let exactish = matching_schema_verbs(verb, known_verbs);
    if !exactish.is_empty() {
        return exactish;
    }

    let normalized = verb.trim();
    if normalized.is_empty() {
        return Vec::new();
    }
    let bare = verb_without_flags(normalized);
    if bare.is_empty() {
        return Vec::new();
    }

    known_verbs
        .iter()
        .copied()
        .filter(|known| {
            known.starts_with(normalized) || verb_without_flags(known).starts_with(&bare)
        })
        .take(5)
        .collect()
}

fn push_unique_command(commands: &mut Vec<String>, command: String) {
    if !commands.iter().any(|existing| existing == &command) {
        commands.push(command);
    }
}

fn matching_schema_verbs<'a>(verb: &str, known_verbs: &'a [&'a str]) -> Vec<&'a str> {
    let normalized = verb.trim();
    if normalized.is_empty() {
        return Vec::new();
    }
    let bare = verb_without_flags(normalized);
    let prefix = format!("{bare} ");
    known_verbs
        .iter()
        .copied()
        .filter(|known| {
            *known != normalized
                && (*known == bare
                    || known.starts_with(&prefix)
                    || verb_without_flags(known) == bare)
        })
        .collect()
}

fn normalize_schema_verb_parts(parts: &[String]) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    let mut iter = parts.iter();
    while let Some(part) = iter.next() {
        match part.as_str() {
            "--no-color" | "-q" | "--quiet" | "-v" | "--verbose" => {}
            "--output" | "--repo" => {
                iter.next()
                    .ok_or_else(|| anyhow!("missing value for `{part}`"))?;
            }
            _ if part.starts_with("--output=") || part.starts_with("--repo=") => {}
            _ => normalized.push(part.clone()),
        }
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// Mirror types
// ---------------------------------------------------------------------------
//
// Each mirror struct mirrors the JSON wire shape of a single
// `--output json`-emitting verb. The struct's `serde` attributes match the
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
    Materialized,
    Virtualized,
    Solid,
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
pub struct GitIndexInfoSchema {
    pub commit_mode: String,
    pub has_staged_changes: bool,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
    pub will_commit: Vec<String>,
    pub preserved_after_commit: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GenericJsonObjectSchema {
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexSchema {
    pub output_kind: String,
    pub present: bool,
    pub path: String,
    pub file_entries: usize,
    pub directory_entries: usize,
    pub untracked_directory_entries: usize,
    pub snapshot_bytes: u64,
    pub journal_bytes: u64,
    pub journal_ops: usize,
    pub journal_replay_ms: u128,
    pub dump: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub file: String,
    pub context: Vec<BlameContextSnippetSchema>,
    pub lines: Vec<BlameLineSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameLineSchema {
    pub line_number: usize,
    pub content: String,
    pub change_id: String,
    pub principal: BlamePrincipalSchema,
    pub agent: Option<BlameAgentSchema>,
    pub timestamp: String,
    pub origins: Option<Vec<BlameOriginSchema>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameOriginSchema {
    pub change_id: String,
    pub principal: BlamePrincipalSchema,
    pub agent: Option<BlameAgentSchema>,
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlamePrincipalSchema {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameAgentSchema {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameContextSnippetSchema {
    pub annotation_id: String,
    pub kind: String,
    pub content: String,
    pub revision_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FsckSchema {
    pub valid: bool,
    pub errors: Vec<FsckErrorSchema>,
    pub warnings: Vec<String>,
    pub objects_checked: usize,
    pub bridge_checked: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FsckErrorSchema {
    pub kind: String,
    pub message: String,
    pub object: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ResolveSchema {
    pub message: Option<String>,
    pub resolved: Option<Vec<String>>,
    pub remaining: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
}

#[allow(dead_code, clippy::large_enum_variant)]
#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum InspectSchema {
    #[allow(dead_code)]
    State(ShowSchema),
    #[allow(dead_code)]
    Thread(ThreadShowSchema),
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroSchema {
    pub since: Option<String>,
    pub until: Option<String>,
    pub duration_secs: Option<i64>,
    pub states_captured: Vec<RetroStateEntrySchema>,
    pub agents_active: Vec<RetroAgentEntrySchema>,
    pub markers_created: Vec<RetroMarkerEntrySchema>,
    pub context_annotations: Vec<RetroContextAnnotationEntrySchema>,
    pub verify_signals: Vec<RetroVerifySignalSchema>,
    pub merges: Vec<RetroOperationEntrySchema>,
    pub undos: Vec<RetroOperationEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroStateEntrySchema {
    pub change_id: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub agent: Option<String>,
    pub principal: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroAgentEntrySchema {
    pub session_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub tokens: RetroAgentTokensSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroAgentTokensSchema {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub tool_calls: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroMarkerEntrySchema {
    pub name: String,
    pub state: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroContextAnnotationEntrySchema {
    pub path: String,
    pub scope: String,
    pub kind: String,
    pub content_excerpt: String,
    pub attribution: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroVerifySignalSchema {
    pub kind: String,
    pub label: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroOperationEntrySchema {
    pub description: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscussionSchema {
    pub id: String,
    pub file: String,
    pub symbol: String,
    pub opened_against_state: String,
    pub opened_at_secs: i64,
    pub visibility: String,
    pub body_changed_since_open: bool,
    pub orphaned: bool,
    pub resolution: DiscussionResolutionSchema,
    pub turns: Vec<DiscussionTurnSchema>,
    pub resolved_annotation_id: Option<String>,
}

/// Per-discussion verbs (`open`/`append`/`resolve`/`show`) emit the
/// discussion payload flattened beneath an `output_kind` discriminator,
/// mirroring `DiscussionEnvelope` in `discuss.rs`. `discuss list` reuses
/// the bare [`DiscussionSchema`] for its inner items — those carry no
/// per-item discriminator (the list envelope owns it), so the
/// discriminator lives on this wrapper rather than on the shared inner
/// struct.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscussionEnvelopeSchema {
    pub output_kind: String,
    #[serde(flatten)]
    pub discussion: DiscussionSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscussionResolutionSchema {
    pub kind: String,
    pub annotation_id: Option<String>,
    pub state_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscussionTurnSchema {
    pub author_name: String,
    pub author_email: String,
    pub body: String,
    pub posted_at_secs: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiscussionListSchema {
    pub output_kind: String,
    pub discussions: Vec<DiscussionSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct QuerySchema {
    pub output_kind: String,
    pub hits: Vec<QueryHitSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct QueryHitSchema {
    pub seq: u64,
    pub timestamp_secs: i64,
    pub verb: String,
    pub actor_email: String,
    pub operation_id: Option<String>,
    pub thread: Option<String>,
    pub symbols: Vec<String>,
    pub signal_kinds: Vec<String>,
    pub change_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OperationRecordSchema {
    pub op_id: String,
    pub command: String,
    pub idempotency_status: String,
    pub replayed: bool,
}

// ---- core loop write/read helpers -----------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct InitSchema {
    pub output_kind: String,
    pub status: String,
    pub action: String,
    pub path: String,
    pub repository_mode: String,
    pub git_detected: bool,
    pub heddle_initialized: bool,
    pub installed_heddleignore: bool,
    pub principal_configured: bool,
    pub principal_status: String,
    pub principal_source: Option<String>,
    pub principal: Option<InitPrincipalSchema>,
    pub principal_recommended_action: Option<String>,
    pub side_effects: Vec<String>,
    pub message: String,
    pub next_action: Option<String>,
    pub recommended_action: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct InitPrincipalSchema {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CaptureSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub principal: CommitPrincipalSchema,
    pub agent: Option<CommitAgentSchema>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    pub signed: bool,
    pub message: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub change_id: String,
    pub git_commit: Option<String>,
    pub git_previous_commit: Option<String>,
    pub summary: String,
    pub confidence: Option<f32>,
    pub git_index: Option<GitIndexInfoSchema>,
    pub included_pending_capture: Option<String>,
    pub principal: CommitPrincipalSchema,
    pub agent: Option<CommitAgentSchema>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub op_id: Option<String>,
    pub operation_record: Option<OperationRecordSchema>,
    pub idempotency_status: Option<String>,
    pub replayed: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitPrincipalSchema {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitAgentSchema {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckpointSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub change_id: String,
    pub git_commit: String,
    pub summary: String,
    pub capability: String,
    pub storage_model: String,
    pub committed_at: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OperatorCommandSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub message: String,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UndoSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub action: String,
    pub message: String,
    pub batches: Vec<Value>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    /// heddle#305: the pre-undo state preserved for recovery, and the marker
    /// pointing at it. Present only on a completed `undo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_marker: Option<String>,
}

/// `heddle undo --list --output json` history view. Distinct from
/// [`UndoSchema`] (the rewind/redo payload): the list view carries only the
/// discriminator and the oplog batches, with none of the action/status/
/// recovery fields a real undo emits. Mirrors `OpListOutput` in `undo.rs`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UndoListSchema {
    pub output_kind: Option<String>,
    pub batches: Vec<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CleanSchema {
    pub output_kind: String,
    pub removed: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub changed_path_count: usize,
    pub stats: DiffStatsSchema,
    /// Worktree-mode diff (`heddle diff` with no revision args) groups the
    /// per-file changes into `{modified, added, deleted}` category arrays,
    /// mirroring the `status` command's `changes` shape so a UI can derive
    /// add/modify/delete badges from `diff` alone. A state-to-state diff
    /// (`heddle diff <a> <b>`) instead emits a flat `array<object>` here.
    pub changes: DiffChangesSchema,
    pub semantic_changes: Option<Vec<Value>>,
    pub context: Option<Vec<Value>>,
    pub broader_guidance: Option<Vec<Value>>,
    /// Rendered unified-diff text, suitable for `patch(1)` / `git apply`.
    /// Present whenever line-level hunks exist, regardless of the
    /// `--patch` CLI flag — JSON consumers always get a parseable diff.
    pub patch: Option<String>,
}

/// `changes` admits the two documented shapes the `diff` command emits:
/// worktree mode (`heddle diff` with no revision args) groups entries into
/// `{modified, added, deleted}` category arrays; a state-to-state diff
/// (`heddle diff <a> <b>`) emits a flat `array<object>`. The schema is a
/// union of both so either documented output validates.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum DiffChangesSchema {
    /// Worktree-mode: per-file diff entries bucketed by category, mirroring
    /// the `status` command's `{modified, added, deleted}` field names. Each
    /// entry carries its path plus the per-file diff fields (`kind`,
    /// `old_path`, `lines`, …). A `renamed` entry buckets under `modified`
    /// (its `kind`/`old_path` identify the rename).
    Grouped(DiffChangesGroupedSchema),
    /// State-to-state: a flat array of per-file diff entries.
    Flat(Vec<Value>),
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffChangesGroupedSchema {
    pub modified: Vec<Value>,
    pub added: Vec<Value>,
    pub deleted: Vec<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffStatsSchema {
    pub files_changed: usize,
    pub additions: usize,
    pub modifications: usize,
    pub deletions: usize,
    pub renames: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GotoSchema {
    pub output_kind: String,
    pub target: String,
    pub intent: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BranchCompatSchema {
    pub output_kind: Option<String>,
    pub repository_capability: Option<String>,
    pub storage_model: Option<String>,
    pub hosted_enabled: Option<bool>,
    pub threads: Option<Vec<ThreadSummarySchema>>,
    pub current: Option<String>,
    pub recommended_action: Option<String>,
    pub recovery_commands: Option<Vec<String>>,
    pub name: Option<String>,
    pub message: Option<String>,
    pub thread: Option<ThreadSummarySchema>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    #[serde(rename = "verification")]
    pub trust: Option<RepositoryVerificationStateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SwitchCheckoutSchema {
    pub output_kind: Option<String>,
    pub name: Option<String>,
    pub message: String,
    pub thread: Option<ThreadSummarySchema>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    pub target: Option<String>,
    pub intent: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MergePreviewSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub action: Option<String>,
    pub message: Option<String>,
    pub would_merge: bool,
    pub blockers: Option<Vec<String>>,
    pub warnings: Option<Vec<String>>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub fast_forward: Option<bool>,
    pub preview_only: Option<bool>,
    pub merge_state: Option<String>,
    pub conflicts: Option<Vec<String>>,
    pub preview_summary: Option<Vec<String>>,
    pub thread_state: Option<String>,
    pub freshness: Option<String>,
    pub changed_paths: Option<Vec<String>>,
    pub changed_path_count: Option<usize>,
    pub impact_categories: Option<Vec<String>>,
    pub promotion_suggested: Option<bool>,
    pub heavy_impact_paths: Option<Vec<String>>,
    pub semantic_result: Option<String>,
    pub conflict_count: Option<usize>,
    pub thread_health: Option<String>,
    pub diff: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AttemptSchema {
    pub status: String,
    pub action: String,
    pub message: String,
    pub command: String,
    pub evaluate: Option<String>,
    pub attempts_total: usize,
    pub attempts_succeeded: usize,
    pub attempts_dropped: usize,
    pub attempts: Vec<AttemptResultSchema>,
    pub recommended: Option<String>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AttemptResultSchema {
    pub index: usize,
    pub thread: String,
    pub status: String,
    pub primary_exit_code: Option<i32>,
    pub primary_duration_secs: f64,
    pub evaluate_exit_code: Option<i32>,
    pub evaluate_duration_secs: Option<f64>,
    pub captured_state: Option<String>,
    pub diff_files: Option<usize>,
    pub thread_dropped: bool,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadySchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub message: String,
    pub blockers: Option<Vec<String>>,
    pub warnings: Option<Vec<String>>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub captured: bool,
    pub captured_state: Option<String>,
    pub thread_state: Option<String>,
    pub report: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SyncSchema {
    #[serde(flatten)]
    pub operator: OperatorCommandSchema,
    pub thread: Option<String>,
    pub current_state: Option<String>,
    pub chosen_path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LandSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub message: String,
    pub blockers: Option<Vec<String>>,
    pub warnings: Option<Vec<String>>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub thread: String,
    pub captured: bool,
    pub checkpointed: bool,
    pub git_commit: Option<String>,
    pub synced: bool,
    pub integrated: bool,
    pub pushed: bool,
    pub pushed_remote: Option<String>,
    pub performed_steps: Vec<String>,
    pub skipped_steps: Vec<String>,
    pub merge_state: Option<String>,
    pub chosen_path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DelegateSchema {
    pub parent_thread: String,
    pub delegated: Vec<DelegatedThreadSchema>,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DelegatedThreadSchema {
    pub name: String,
    pub task: String,
    pub path: Option<String>,
    pub execution_path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadStartSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub action: Option<String>,
    pub name: String,
    pub message: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub thread: Option<ThreadSummarySchema>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadCurrentSchema {
    pub thread: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ThreadCapturesSchema(pub Vec<ThreadCaptureEntrySchema>);

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadCaptureEntrySchema {
    pub change_id: String,
    pub created_at: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub agent: Option<String>,
    pub message: String,
    pub summary: Option<ThreadCaptureSummarySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadCaptureSummarySchema {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub total: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadCommandSchema {
    pub output_kind: String,
    pub status: String,
    pub action: String,
    pub name: String,
    pub message: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub thread: Option<ThreadSummarySchema>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadMoveSchema {
    pub from_thread: String,
    pub to_thread: String,
    pub moved_paths: Vec<String>,
    pub source_change_id: Option<String>,
    pub target_change_id: String,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadAbsorbSchema {
    pub thread: String,
    pub into: String,
    pub preview_only: bool,
    pub conflicts: Vec<String>,
    pub merge_state: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadResolveSchema {
    #[serde(flatten)]
    pub operator: OperatorCommandSchema,
    pub thread: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadApprovalSchema {
    pub id: String,
    pub repo_path: String,
    pub source_thread: String,
    pub target_thread: String,
    pub source_state: String,
    pub approver_user_id: String,
    pub note: String,
    pub approved_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ThreadApprovalListSchema(pub Vec<ThreadApprovalSchema>);

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadRevokeApprovalSchema {
    pub output_kind: String,
    pub deleted: bool,
    pub id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadMergeEligibilitySchema {
    pub allowed: bool,
    pub unmet: Vec<ThreadMergeRequirementSchema>,
    pub valid_approvals: Vec<ThreadApprovalSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadMergeRequirementSchema {
    pub policy_id: String,
    pub kind: String,
    pub group_id: String,
    pub reason: String,
    pub needed: u32,
    pub have: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadCleanupSchema {
    #[serde(flatten)]
    pub operator: OperatorCommandSchema,
    pub dry_run: bool,
    pub merged: Vec<ThreadDroppedSchema>,
    pub auto: Vec<ThreadDroppedSchema>,
    pub reclaimed_bytes: u64,
    pub would_reclaim_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<ThreadCleanupSkippedSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadDroppedSchema {
    pub thread: String,
    pub id: String,
    pub reason: String,
    pub age_seconds: i64,
    pub bytes: u64,
    pub execution_path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadCleanupSkippedSchema {
    pub thread: String,
    pub id: String,
    pub reason: String,
    pub note: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadShowSchema {
    pub output_kind: Option<String>,
    pub repository_label: String,
    pub repository_context: Option<RepositoryContextInfoSchema>,
    #[serde(flatten)]
    pub summary: ThreadSummarySchema,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
    pub recovery_commands: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadSummarySchema {
    pub name: String,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    pub actor: Option<ActorInfoSchema>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_confidence: Option<f32>,
    pub usage_summary: OpaqueObject,
    pub last_progress_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub report_flush_state: Option<String>,
    pub attach_reason: Option<String>,
    pub thread_mode: Option<ThreadModeSchema>,
    pub thread_state: Option<ThreadStateSchema>,
    pub freshness: Option<ThreadFreshnessSchema>,
    pub visibility: String,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    pub sibling_threads: Vec<String>,
    pub stack_depth: usize,
    pub stale_from_parent: bool,
    pub task: Option<String>,
    pub changed_paths: Vec<String>,
    pub promotion_suggested: bool,
    pub impact_categories: Vec<ThreadImpactCategorySchema>,
    pub heavy_impact_paths: Vec<String>,
    pub verification_summary: Value,
    pub confidence_summary: Value,
    pub integration_policy_result: Value,
    pub coordination_status: CoordinationStatusSchema,
    pub is_current: bool,
    pub is_isolated: bool,
    pub thread_health: String,
    pub blockers: Vec<String>,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub git_branch_tip: Option<String>,
    pub history_imported: bool,
    pub auto: bool,
    pub shared_target_dir: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CloneSchema {
    pub output_kind: Option<String>,
    pub action: Option<String>,
    pub status: Option<String>,
    pub success: Option<bool>,
    pub cloned: Option<bool>,
    pub transport: Option<String>,
    pub remote: Option<String>,
    pub local: Option<String>,
    pub branch: Option<String>,
    pub repository_capability: Option<String>,
    pub commits_imported: Option<u64>,
    pub states_created: Option<u64>,
    pub objects: Option<usize>,
    pub state: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AdoptSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub action: Option<String>,
    pub adopted: bool,
    pub initialized: bool,
    pub path: String,
    pub refs: Vec<String>,
    pub commits_imported: usize,
    pub states_created: usize,
    pub branches_synced: usize,
    pub tags_synced: usize,
    pub skipped_non_commit_refs: usize,
    pub partial_mirror_refs: usize,
    pub already_in_sync: bool,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RemoteListSchema {
    pub output_kind: Option<String>,
    pub remotes: Vec<RemoteInfoSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RemoteInfoSchema {
    pub output_kind: Option<String>,
    pub name: String,
    pub url: String,
    pub source: String,
    pub is_default: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RemoteMutationSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: String,
    pub name: String,
    pub url: Option<String>,
    pub default: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorSingleSchema {
    pub actor: ActorEntrySchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorListSchema {
    pub actors: Vec<ActorEntrySchema>,
    pub active_only: bool,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorDoneSchema {
    pub session_id: String,
    pub status: String,
    pub thread: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorExplainSchema {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_actor: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action_template: Option<ActionTemplateSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_instance_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_precedence: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winning_rule: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorEntrySchema {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_instance_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    pub thread: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub base_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    pub usage_summary: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_flush_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_reason: Option<String>,
    pub attach_precedence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winning_attach_rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_confidence: Option<f32>,
    pub status: String,
    pub started_at: String,
    pub actor_chain: Vec<ActorChainEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorChainEntrySchema {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    pub thread: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentServeSchema {
    pub output_kind: String,
    pub status: String,
    pub socket_path: String,
    pub pid_path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentDaemonStatusSchema {
    pub output_kind: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_path: String,
    pub pid_path: String,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentStopSchema {
    pub output_kind: String,
    pub stopped: bool,
    pub swept_stale: bool,
    pub pid: Option<i32>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentReservationEnvelopeSchema {
    pub reservation: AgentReservationSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentReservationListSchema {
    pub reservations: Vec<AgentReservationSchema>,
    pub alive_only: bool,
    pub thread: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentReservationSchema {
    pub session_id: String,
    pub reservation_token: Option<String>,
    pub thread: String,
    pub anchor_state: Option<String>,
    pub anchor_root: Option<String>,
    pub status: String,
    pub path: Option<String>,
    pub task: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub probe_source: Option<String>,
    pub probe_confidence: Option<f32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionEnvelopeSchema {
    pub session: SessionEntrySchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionSegmentEnvelopeSchema {
    pub segment: SessionSegmentSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionListSchema {
    pub sessions: Vec<SessionEntrySchema>,
    pub active_only: bool,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionEntrySchema {
    pub id: String,
    pub principal: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    pub active: bool,
    pub segments: Vec<SessionSegmentSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionSegmentSchema {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FetchSchema {
    pub output_kind: Option<String>,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_included: Option<bool>,
    pub refs_fetched: usize,
    pub objects_fetched: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PullSchema {
    pub output_kind: Option<String>,
    pub action: Option<String>,
    pub status: Option<String>,
    pub pulled: Option<bool>,
    pub changed: Option<bool>,
    pub success: Option<bool>,
    pub transport: Option<String>,
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_git_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_git_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub state: Option<String>,
    pub objects: Option<usize>,
    pub states_created: Option<usize>,
    pub commits_seen: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_seen_scope: Option<String>,
    pub materialized_checkout: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_path_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PushSchema {
    pub output_kind: String,
    pub action: String,
    pub status: String,
    pub pushed: bool,
    pub changed: bool,
    pub success: bool,
    pub transport: String,
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_notes_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_notes_visibility_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_tracking_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote_configured: Option<GitRemoteConfiguredSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_upstream_configured: Option<GitUpstreamConfiguredSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_included: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_discard_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub state: Option<String>,
    pub objects: Option<usize>,
    #[schemars(required)]
    pub next_action: Option<String>,
    #[schemars(required)]
    pub next_action_template: Option<ActionTemplateSchema>,
    #[schemars(required)]
    pub recommended_action: Option<String>,
    #[schemars(required)]
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitRemoteConfiguredSchema {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitUpstreamConfiguredSchema {
    pub branch: String,
    pub remote: String,
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

#[derive(Debug, Serialize, JsonSchema)]
pub struct RepositoryContextInfoSchema {
    pub kind: String,
    pub parent_repository: Option<String>,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
}

// ---- status ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusSchema {
    pub output_kind: Option<String>,
    pub repository_capability: String,
    pub repository_label: String,
    pub repository_context: Option<RepositoryContextInfoSchema>,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub git_overlay_health: GitOverlayHealthSchema,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
    pub thread: Option<String>,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    pub worktree_changed_path_count: usize,
    pub thread_changed_path_count: usize,
    pub blockers: Vec<String>,
    pub recommended_action: NullableStringSchema,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
    pub thread_health: String,
    pub coordination_status: CoordinationStatusSchema,
    pub is_isolated: bool,
    pub parallel_threads: Vec<ParallelThreadInfoSchema>,
    pub state: Option<StateInfoSchema>,
    pub git_checkpoint: Option<GitCheckpointInfoSchema>,
    pub changes: ChangesInfoSchema,
    pub git_index: Option<GitIndexInfoSchema>,
}

// ---- verify ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct VerifySchema {
    pub output_kind: String,
    pub clean: bool,
    pub repository_label: String,
    pub repository_context: Option<RepositoryContextInfoSchema>,
    #[serde(flatten)]
    pub verification: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RepositoryVerificationStateSchema {
    #[serde(rename = "verified")]
    pub verified: bool,
    pub status: String,
    pub repository_mode: String,
    pub heddle_initialized: bool,
    pub git_branch: Option<String>,
    pub heddle_thread: Option<String>,
    pub worktree_dirty: bool,
    pub worktree_state: String,
    pub import_state: String,
    pub mapping_state: String,
    pub remote_drift: String,
    pub active_operation: Option<String>,
    pub default_remote: Option<String>,
    pub clone_verification: String,
    pub machine_contract: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_contract_coverage: Option<MachineContractCoverageSchema>,
    pub workflow_status: String,
    pub workflow_summary: String,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
    pub checks: Vec<VerificationCheckSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MachineContractCoverageSchema {
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
    pub schema_verbs_total: usize,
    pub documented_schema_verbs_total: usize,
    pub undocumented_schema_verbs_total: usize,
    pub opaque_schema_verbs_total: usize,
    pub accepted_opaque_schema_verbs_total: usize,
    pub unaccepted_opaque_schema_verbs_total: usize,
    pub supports_op_id_total: usize,
    pub jsonl_commands_total: usize,
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

#[derive(Debug, Serialize, JsonSchema)]
pub struct VerificationCheckSchema {
    pub name: String,
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
    pub details: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActionTemplateSchema {
    pub action: String,
    pub argv_template: Vec<String>,
    pub required_inputs: Vec<String>,
    pub agent_may_fill: bool,
}

// ---- bridge git status ----------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeGitStatusSchema {
    pub output_kind: Option<String>,
    pub repository_capability: String,
    pub storage_model: String,
    pub mirror_path: Option<String>,
    pub mirror_initialized: bool,
    pub git_overlay_import_hint: Option<GitOverlayImportHintSchema>,
    pub git_overlay_health: GitOverlayHealthSchema,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitOverlayImportHintSchema {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitOverlayHealthSchema {
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recovery_commands: Vec<String>,
    pub checks: Vec<GitOverlayHealthCheckSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitOverlayHealthCheckSchema {
    pub name: String,
    pub status: String,
    pub summary: String,
}

// ---- log ------------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct LogSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
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
    pub output_kind: Option<String>,
    pub status: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    pub output_kind: Option<String>,
    pub repository_capability: String,
    pub repository_label: String,
    pub repository_context: Option<RepositoryContextInfoSchema>,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub threads: Vec<ThreadSummarySchema>,
    pub available_git_refs: Vec<AvailableGitRefSchema>,
    pub current: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AvailableGitRefSchema {
    pub name: String,
    pub git_commit: String,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

// ---- workspace show -------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkspaceShowSchema {
    pub output_kind: Option<String>,
    pub repository: String,
    pub repository_capability: String,
    pub repository_label: String,
    pub repository_context: Option<RepositoryContextInfoSchema>,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub current_thread: Option<String>,
    pub groups: Vec<WorkspaceGroupSchema>,
    pub available_git_refs: Vec<AvailableGitRefSchema>,
    pub thread_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkspaceGroupSchema {
    pub id: String,
    pub label: String,
    pub threads: Vec<ThreadSummarySchema>,
}

// ---- review ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewShowSchema {
    pub output_kind: String,
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
    pub output_kind: String,
    pub signature_id: String,
    pub change_id: String,
}

/// `heddle review next --output json` emits a stable envelope keyed by
/// `output_kind: "review_next"`. When the scan window holds a pending
/// review, the pending state's view is flattened alongside `output_kind`
/// (`change_id`, `headline`, `existing_signatures`) and the same view is
/// echoed under `next`. When no pending review is found, only
/// `output_kind` and `next: null` are emitted — there is no top-level
/// `null`. Mirrors the envelope built in `review::run_next`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewNextSchema {
    pub output_kind: String,
    pub change_id: Option<String>,
    pub headline: Option<String>,
    pub existing_signatures: Option<u32>,
    pub next: RequiredNullableNextState,
}

/// `next` is ALWAYS present in the runtime envelope — either the pending
/// review state or an explicit JSON `null`. Modeling it as
/// `Option<ReviewNextStateSchema>` directly would let schemars drop the
/// field from the schema's `required` set, advertising a shape the command
/// never emits. This wrapper keeps the value nullable (its schema delegates
/// to `Option`'s nullable form) while reporting `_schemars_private_is_option
/// == false`, so the derive marks `next` required (heddle#272 Codex r7).
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct RequiredNullableNextState(pub Option<ReviewNextStateSchema>);

impl JsonSchema for RequiredNullableNextState {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("RequiredNullableNextState")
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <Option<ReviewNextStateSchema> as JsonSchema>::json_schema(generator)
    }
}

/// The pending review state echoed under `review next`'s `next` field.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewNextStateSchema {
    pub change_id: String,
    pub headline: String,
    pub existing_signatures: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewHealthSchema {
    pub output_kind: String,
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

// ---- command/schema introspection ----------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct SchemasListSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub schema_verbs: Vec<String>,
    pub documented_schema_verbs: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoctorDocsSchema {
    pub output_kind: String,
    pub status: String,
    #[serde(rename = "verified")]
    pub verified: bool,
    pub recommended_action: Option<String>,
    pub files_scanned: usize,
    pub issues: Vec<DoctorDocsIssueSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoctorDocsIssueSchema {
    pub file: String,
    pub line: usize,
    pub invocation: String,
    pub kind: String,
    pub detail: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoctorSchemasSchema {
    pub output_kind: String,
    pub status: String,
    #[serde(rename = "verified")]
    pub verified: bool,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recovery_commands: Vec<String>,
    pub registered_verbs: Vec<String>,
    pub documented_verbs: Vec<String>,
    pub undocumented_verbs: Vec<String>,
    pub unmatched_verbs: Vec<String>,
    pub passing_verbs: Vec<String>,
    pub issues: Vec<DoctorSchemaIssueSchema>,
    pub command_contract_schema_coverage: CommandContractSchemaCoverageSchema,
    pub doc_path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoctorSchemaIssueSchema {
    pub verb: String,
    pub line: usize,
    pub unknown_key: String,
    pub detail: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommandContractSchemaCoverageSchema {
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

#[derive(Debug, Serialize, JsonSchema)]
pub struct GitOverlayGuideSchema {
    pub topic: String,
    pub summary: String,
    pub steps: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WatchLineSchema {
    pub ts: String,
    pub thread: Option<String>,
    pub kind: String,
    pub change_id: Option<String>,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub actor: Option<ActorInfoSchema>,
    pub id: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TrySchema {
    pub status: String,
    pub action: String,
    pub message: String,
    pub thread: String,
    pub thread_dropped: bool,
    pub cleanup_error: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub captured_state: Option<String>,
    pub merge_state: Option<String>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
}

// ---- bridge ops -----------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeInitSchema {
    pub initialized: bool,
    pub path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExportedRefSchema {
    pub name: String,
    pub tip: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeExportSchema {
    pub states_exported: u64,
    pub commits_total: u64,
    pub threads_synced: u64,
    pub markers_synced: u64,
    pub branches: Vec<ExportedRefSchema>,
    pub tags: Vec<ExportedRefSchema>,
    pub destination: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeImportSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: Option<String>,
    pub summary: String,
    pub commits_imported: u64,
    pub states_created: u64,
    pub branches_synced: u64,
    pub tags_synced: u64,
    pub skipped_non_commit_refs: u64,
    pub partial_mirror_refs: u64,
    pub lossy_entries: Vec<LossyGitImportEntrySchema>,
    pub already_in_sync: bool,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LossyGitImportEntrySchema {
    pub path: String,
    pub action: String,
    pub reason: String,
    pub git_object: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeSyncSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: Option<String>,
    pub summary: String,
    pub states_exported: u64,
    pub commits_exported_total: u64,
    pub commits_imported: u64,
    pub threads_synced: u64,
    pub markers_synced: u64,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgeGitReconcileSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: Option<String>,
    pub prefer: Option<String>,
    pub ref_name: String,
    pub preview: bool,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgePushSchema {
    pub output_kind: Option<String>,
    pub action: Option<String>,
    pub status: Option<String>,
    pub success: Option<bool>,
    pub pushed: bool,
    pub changed: Option<bool>,
    pub transport: Option<String>,
    pub remote: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BridgePullSchema {
    pub output_kind: Option<String>,
    pub action: Option<String>,
    pub status: Option<String>,
    pub success: Option<bool>,
    pub pulled: bool,
    pub changed: Option<bool>,
    pub transport: Option<String>,
    pub remote: String,
}

// ---- stash / revert -------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashMutationSchema {
    pub message: String,
    pub stash_index: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashListSchema {
    pub output_kind: String,
    pub stashes: Vec<StashListEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashListEntrySchema {
    pub index: usize,
    pub message: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashShowSchema {
    pub output_kind: String,
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RevertSchema {
    pub output_kind: String,
    pub change_id: Option<String>,
    pub reverted_state: String,
    pub files_affected: Vec<String>,
    pub message: String,
}

// ---- diagnose -------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiagnoseSchema {
    pub output_kind: Option<String>,
    pub repository: String,
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub git_overlay_import_hint: Option<GitOverlayImportHintSchema>,
    pub git_overlay_health: GitOverlayHealthSchema,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub thread: Option<Value>,
    pub state: Option<Value>,
    pub changes: Value,
    pub workspace: Value,
    pub health: Value,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
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
// - `code` — stable machine code; currently mirrors `kind`.
// - `error` — human-readable message (the anyhow chain rendered via `{:#}`).
//   Always present, never empty.
// - `exit_code` — process exit code emitted for the failure.
// - `hint` — single-line next-step recommendation. Empty string when no
//   actionable hint applies. JSON-mode runtime errors use a non-empty
//   fallback hint when no specific recovery class applies.
// - `kind` — stable predicate name keying the hint family. JSON-mode
//   runtime errors use `runtime_error` when the error didn't match a
//   known class. Current values include:
//   `repository_not_found`, `repository_exists`, `state_not_found`,
//   `thread_not_found`, `out_of_space`, `permission_denied`,
//   `read_only_filesystem`, and `runtime_error`. New kinds may be added
//   (additive); existing ones are stable.
// - `unsafe_condition`, `would_change`, `preserved` — typed safety facts.
// - `primary_command`, `primary_command_template` — the main recovery
//   action as a human-readable command string plus a fillable template
//   (always present for a valid action). The `_argv` sidecar was dropped
//   (HeddleCo/heddle#254): it was null for every placeholder action and
//   silently read as "no action" to agents — use the template instead.
// - `recovery_commands`, `recovery_action_templates` — all recovery
//   actions the runtime can represent, as command strings or fillable
//   templates.

#[derive(Debug, Serialize, JsonSchema)]
pub struct ErrorEnvelopeSchema {
    pub code: String,
    pub error: String,
    pub exit_code: u8,
    pub hint: String,
    pub kind: String,
    pub op_id: Option<String>,
    pub idempotency_status: Option<String>,
    pub replayed: Option<bool>,
    pub unsafe_condition: String,
    pub would_change: String,
    pub preserved: String,
    pub primary_command: String,
    pub primary_command_template: NullableActionTemplateSchema,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum NullableActionTemplateSchema {
    Template(ActionTemplateSchema),
    Null(()),
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum NullableStringSchema {
    Value(String),
    Null(()),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn required_fields(schema: &Value) -> Vec<&str> {
        schema
            .get("required")
            .and_then(|value| value.as_array())
            .expect("schema has required fields")
            .iter()
            .map(|value| value.as_str().expect("required field is a string"))
            .collect()
    }

    fn property_schema<'a>(schema: &'a Value, property: &str) -> &'a Value {
        schema
            .get("properties")
            .and_then(|p| p.as_object())
            .and_then(|properties| properties.get(property))
            .unwrap_or_else(|| panic!("schema has `{property}` property"))
    }

    fn schema_allows_null(root: &Value, schema: &Value) -> bool {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            let definition = reference
                .strip_prefix("#/$defs/")
                .or_else(|| reference.strip_prefix("#/definitions/"))
                .and_then(|name| {
                    root.get("$defs")
                        .or_else(|| root.get("definitions"))
                        .and_then(|defs| defs.get(name))
                })
                .unwrap_or_else(|| panic!("schema reference `{reference}` resolves"));
            return schema_allows_null(root, definition);
        }

        if schema.get("type") == Some(&Value::String("null".to_string())) {
            return true;
        }
        if schema
            .get("type")
            .and_then(|value| value.as_array())
            .is_some_and(|types| types.contains(&Value::String("null".to_string())))
        {
            return true;
        }

        ["anyOf", "oneOf", "allOf"].iter().any(|combinator| {
            schema
                .get(*combinator)
                .and_then(|value| value.as_array())
                .is_some_and(|schemas| {
                    schemas
                        .iter()
                        .any(|schema| schema_allows_null(root, schema))
                })
        })
    }

    fn collect_string_enums<'a>(root: &'a Value, schema: &'a Value, values: &mut Vec<&'a str>) {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            let definition = reference
                .strip_prefix("#/$defs/")
                .or_else(|| reference.strip_prefix("#/definitions/"))
                .and_then(|name| {
                    root.get("$defs")
                        .or_else(|| root.get("definitions"))
                        .and_then(|defs| defs.get(name))
                })
                .unwrap_or_else(|| panic!("schema reference `{reference}` resolves"));
            collect_string_enums(root, definition, values);
        }

        if let Some(enum_values) = schema.get("enum").and_then(|value| value.as_array()) {
            for value in enum_values {
                if let Some(value) = value.as_str() {
                    values.push(value);
                }
            }
        }

        for combinator in ["anyOf", "oneOf", "allOf"] {
            if let Some(schemas) = schema.get(combinator).and_then(|value| value.as_array()) {
                for schema in schemas {
                    collect_string_enums(root, schema, values);
                }
            }
        }
    }

    /// Every schema verb advertised by the command contract table must
    /// produce a schema.
    /// Otherwise `heddle doctor schemas` would silently miss drift on
    /// that verb.
    #[test]
    fn registry_covers_every_listed_verb() {
        for verb in schema_verbs() {
            assert!(
                schema_for_verb(verb).is_some(),
                "verb '{verb}' is advertised by command contracts but schema_for_verb returned None"
            );
        }
    }

    #[test]
    fn documented_registry_is_subset_of_runtime_registry() {
        let all = schema_verbs();
        for verb in documented_schema_verbs() {
            assert!(
                all.contains(verb),
                "documented schema verb '{verb}' is not advertised as a runtime schema"
            );
        }
    }

    /// Every documented (non-opaque) verb whose catalog advertises an
    /// `output_kind` discriminator must declare the `output_kind`
    /// property on its *registered schema struct*, not merely rely on the
    /// runtime injection in [`schema_for_verb`].
    ///
    /// heddle#272 r6 (Codex P2): `schema_for_verb` injects the
    /// discriminator from the catalog after deriving the struct schema,
    /// so `heddle schemas <verb>` already surfaces `output_kind`. That
    /// injection masks the fact that the Rust mirror struct (e.g.
    /// `CleanSchema`, `GotoSchema`) never declares the field. The mirror
    /// is the source of truth a reader greps; it must be honest about the
    /// discriminator the runtime always emits. This check reads the
    /// *pre-injection* struct schema so a missing field fails CI rather
    /// than being papered over by the catalog.
    #[test]
    fn documented_swept_schema_structs_declare_output_kind() {
        let mut missing = Vec::new();
        for verb in documented_schema_verbs() {
            // Opaque verbs expose a generic object schema; their
            // discriminator is genuinely catalog-only (there is no
            // Serialize mirror struct to declare it on).
            if opaque_schema_verbs().contains(verb) {
                continue;
            }
            let Some(discriminator) =
                command_catalog::command_json_discriminator_for_schema_verb(verb)
            else {
                continue;
            };
            if discriminator.field != "output_kind" {
                continue;
            }
            let bare = schema_for_registered_verb(verb)
                .unwrap_or_else(|| panic!("documented verb `{verb}` has no registered schema"));
            let declares = bare
                .get("properties")
                .and_then(|properties| properties.get("output_kind"))
                .is_some();
            if !declares {
                missing.push(format!(
                    "{verb}: catalog advertises output_kind=`{}` but the schema struct declares no `output_kind` property",
                    discriminator.value
                ));
            }
        }
        assert!(
            missing.is_empty(),
            "Documented swept schema structs missing the `output_kind` property. Add \
             `pub output_kind: String` to each mirror struct so it matches the runtime \
             emission (the catalog injection masks this at the `heddle schemas` layer, \
             but the struct must be honest):\n  - {}",
            missing.join("\n  - ")
        );
    }

    #[test]
    fn implementation_registry_matches_command_contract_registry() {
        let advertised = schema_verbs();
        let mut implemented = schema_implementation_verbs();
        for verb in opaque_schema_verbs() {
            if !implemented.contains(verb) {
                implemented.push(*verb);
            }
            assert!(
                advertised.contains(verb),
                "opaque schema verb '{verb}' must also be advertised by active command contracts"
            );
        }
        for verb in advertised {
            assert!(
                implemented.contains(verb),
                "verb '{verb}' is advertised by command contracts but the schema implementation registry does not handle it"
            );
        }
        for verb in &implemented {
            if cfg!(all(feature = "git-overlay", feature = "semantic")) {
                assert!(
                    advertised.contains(verb),
                    "verb '{verb}' has a schema implementation but is not advertised by active command contracts"
                );
            } else if !advertised.contains(verb) {
                assert!(
                    schema_for_verb(verb).is_none(),
                    "inactive schema implementation '{verb}' must not be publicly resolvable"
                );
            }
        }
    }

    #[test]
    fn command_catalog_schema_verbs_match_schema_list_except_error_envelope() {
        let catalog = command_catalog::build_command_catalog();
        let mut catalog_verbs = catalog
            .commands
            .iter()
            .flat_map(|command| command.schema_verbs.iter().map(String::as_str))
            .collect::<Vec<_>>();
        catalog_verbs.sort_unstable();
        catalog_verbs.dedup();

        let mut listed_verbs = schema_verbs().to_vec();
        listed_verbs.sort_unstable();
        listed_verbs.retain(|verb| *verb != "error");

        assert_eq!(
            catalog_verbs, listed_verbs,
            "`heddle commands --output json` command schema verbs must match `heddle schemas` except for the cross-cutting JSON error envelope"
        );
    }

    #[cfg(not(feature = "git-overlay"))]
    #[test]
    fn native_only_schema_registry_excludes_git_overlay_verbs() {
        let catalog = command_catalog::build_command_catalog();
        for verb in [
            "bridge git status",
            "bridge git init",
            "bridge git import",
            "bridge git export",
            "bridge git sync",
            "bridge git reconcile",
            "bridge git push",
            "bridge git pull",
            "bridge git ingest",
            "bridge git reason",
            "git-overlay",
        ] {
            assert!(
                !schema_verbs().contains(&verb),
                "native-only schema listing must not advertise git-overlay verb `{verb}`"
            );
            assert!(
                !documented_schema_verbs().contains(&verb),
                "native-only documented schema listing must not advertise git-overlay verb `{verb}`"
            );
            assert!(
                schema_for_verb(verb).is_none(),
                "native-only schema lookup must reject git-overlay verb `{verb}`"
            );
            assert!(
                catalog.commands.iter().all(|command| {
                    !command
                        .schema_verbs
                        .iter()
                        .any(|schema_verb| schema_verb == verb)
                        && !command
                            .documented_schema_verbs
                            .iter()
                            .any(|schema_verb| schema_verb == verb)
                }),
                "native-only command catalog must not advertise git-overlay schema verb `{verb}`"
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
    fn status_schema_allows_null_recommended_action() {
        let schema = schema_for_verb("status").expect("status schema");
        let recommended_action = property_schema(&schema, "recommended_action");
        assert!(
            schema_allows_null(&schema, recommended_action),
            "status recommended_action must allow null because empty actions serialize as null: {recommended_action}"
        );

        let required = required_fields(&schema);
        assert!(
            required.contains(&"recommended_action"),
            "status recommended_action should remain a stable emitted field: {schema}"
        );
    }

    #[test]
    fn status_thread_mode_schema_matches_observed_modes() {
        let schema = schema_for_verb("status").expect("status schema");
        let mut values = Vec::new();
        collect_string_enums(
            &schema,
            property_schema(&schema, "thread_mode"),
            &mut values,
        );

        for expected in ["materialized", "virtualized", "solid"] {
            assert!(
                values.contains(&expected),
                "status thread_mode schema missing observed mode `{expected}`: {values:?}"
            );
        }
        assert!(
            !values.contains(&"lightweight"),
            "status thread_mode schema must not advertise removed mode `lightweight`: {values:?}"
        );
    }

    #[test]
    fn ready_schema_does_not_require_omitted_empty_operator_lists() {
        let schema = schema_for_verb("ready").expect("ready schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("ready schema has properties");
        assert!(
            properties.contains_key("blockers"),
            "ready schema should still document blockers when emitted"
        );
        assert!(
            properties.contains_key("warnings"),
            "ready schema should still document warnings when emitted"
        );

        let required = required_fields(&schema);
        for omitted_when_empty in ["blockers", "warnings"] {
            assert!(
                !required.contains(&omitted_when_empty),
                "ready schema must not require `{omitted_when_empty}` because successful JSON output omits empty operator lists: {schema}"
            );
        }
    }

    #[test]
    fn push_schema_requires_stable_runtime_fields() {
        let schema = schema_for_verb("push").expect("push schema");
        let required = required_fields(&schema);
        for stable_field in [
            "output_kind",
            "action",
            "status",
            "pushed",
            "changed",
            "success",
            "transport",
            "next_action",
            "next_action_template",
            "recommended_action",
            "recommended_action_template",
        ] {
            assert!(
                required.contains(&stable_field),
                "push schema must require stable emitted field `{stable_field}`: {schema}"
            );
        }

        for skipped_when_none in [
            "remote",
            "push_scope",
            "ref_scope",
            "git_notes_ref",
            "git_notes_visibility_warning",
            "git_tracking_remote",
            "git_remote_configured",
            "git_upstream_configured",
            "tags_included",
            "force",
            "force_discard_warning",
            "thread",
            "state",
            "objects",
        ] {
            assert!(
                !required.contains(&skipped_when_none),
                "push schema must not require conditionally omitted field `{skipped_when_none}`: {schema}"
            );
        }
    }

    #[test]
    fn advertised_json_discriminators_are_reflected_in_schemas() {
        for discriminator in command_catalog::command_json_discriminators() {
            let Some(schema_verb) = discriminator.schema_verb.as_deref() else {
                continue;
            };
            let schema =
                schema_for_verb(schema_verb).unwrap_or_else(|| panic!("{schema_verb} schema"));
            let properties = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .unwrap_or_else(|| panic!("{schema_verb} schema has properties"));
            let property = properties.get(&discriminator.field).unwrap_or_else(|| {
                panic!(
                    "{schema_verb} schema missing discriminator field `{}`",
                    discriminator.field
                )
            });
            let enum_values = property
                .get("enum")
                .and_then(|value| value.as_array())
                .map(Vec::as_slice);
            assert_eq!(
                enum_values,
                Some([Value::String(discriminator.value.clone())].as_slice()),
                "{schema_verb} schema must narrow `{}` to `{}`",
                discriminator.field,
                discriminator.value
            );

            let required = schema
                .get("required")
                .and_then(|value| value.as_array())
                .unwrap_or_else(|| panic!("{schema_verb} schema has required fields"));
            assert!(
                required.contains(&Value::String(discriminator.field.clone())),
                "{schema_verb} schema must require discriminator field `{}`",
                discriminator.field
            );
        }
    }

    #[test]
    fn oss_recovery_surfaces_do_not_use_opaque_generic_schema() {
        for verb in [
            "blame",
            "fsck",
            "resolve",
            "retro",
            "inspect",
            "discuss open",
            "discuss append",
            "discuss resolve",
            "discuss list",
            "discuss show",
            "query",
        ] {
            assert!(
                !opaque_schema_verbs().contains(&verb),
                "`{verb}` should have a concrete machine-contract schema, not the opaque generic object"
            );
            let schema = schema_for_verb(verb).unwrap_or_else(|| panic!("{verb} schema exists"));
            assert_ne!(
                schema.get("additionalProperties"),
                Some(&Value::Bool(true)),
                "`{verb}` schema should not accept arbitrary top-level fields"
            );
        }
    }

    #[test]
    fn commit_schema_declares_op_id_replay_fields() {
        let schema = schema_for_verb("commit").expect("commit schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("commit schema has properties");
        for required in OP_ID_REPLAY_FIELD_NAMES {
            assert!(
                properties.contains_key(*required),
                "commit schema missing op-id replay property '{required}'"
            );
        }
    }

    #[test]
    fn op_id_supported_schema_verbs_declare_replay_fields() {
        let mut checked = 0;
        for verb in schema_verbs() {
            if !schema_verb_supports_op_id(verb) {
                continue;
            }
            checked += 1;
            let schema =
                schema_for_verb(verb).unwrap_or_else(|| panic!("schema for `{verb}` exists"));
            let properties = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .unwrap_or_else(|| panic!("schema for `{verb}` should expose properties"));
            for required in OP_ID_REPLAY_FIELD_NAMES {
                assert!(
                    properties.contains_key(*required),
                    "schema for op-id-supported verb `{verb}` missing replay property `{required}`"
                );
            }
        }
        assert!(
            checked > 1,
            "op-id schema coverage test should exercise more than commit"
        );
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
