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

use anyhow::{Result, anyhow};
use heddle_core::{DiffReport, FsckReport, QueryReport, StatusReport, VerifyReport};
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::Value;

use super::{RecoveryAdvice, command_catalog};
use crate::cli::{Cli, should_output_json};

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
            let mut verbs = report_contract_schema_verbs().to_vec();
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

#[cfg(test)]
fn report_contract_schema_verbs() -> &'static [&'static str] {
    &[
        QueryReport::CONTRACT.schema_name,
        DiffReport::CONTRACT.schema_name,
        FsckReport::CONTRACT.schema_name,
        StatusReport::CONTRACT.schema_name,
        VerifyReport::CONTRACT.schema_name,
    ]
}

schema_registry! {
    (&["fsck --repair git"], FsckReport),
    (&["init"], InitSchema),
    (&["adopt"], AdoptSchema),
    (&["capture"], CaptureSchema),
    (&["undo", "undo --redo"], UndoSchema),
    (&["undo --list"], UndoListSchema),
    (&["clean"], CleanSchema),
    (&["switch"], SwitchCheckoutSchema),
    (&["merge --preview"], MergePreviewSchema),
    (&["ready"], ReadySchema),
    (&["land"], LandSchema),
    (&["sync"], SyncSchema),
    (&["continue", "abort"], OperatorCommandSchema),
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
    (&["thread marker list"], ThreadMarkerListSchema),
    (&["thread marker create", "thread marker delete", "thread marker show"], ThreadMarkerOpSchema),
    (&["thread show"], ThreadShowSchema),
    (&["clone"], CloneSchema),
    (&["remote list"], RemoteListSchema),
    (&["remote show"], RemoteInfoSchema),
    (&["remote add", "remote remove", "remote set-default"], RemoteMutationSchema),
    (&["fetch"], FetchSchema),
    (&["pull"], PullSchema),
    (&["push"], PushSchema),
    (&["expand"], ExpandSchema),
    (&["log"], LogSchema),
    (&["log --reflog"], LogReflogSchema),
    (&["log --timeline"], TimelineLogSchema),
    (&["timeline status"], TimelineStatusSchema),
    (&["timeline record-start", "timeline record-finish"], TimelineRecordingSchema),
    (&["timeline fork", "timeline reset", "timeline recover"], TimelineActionSchema),
    (&["show"], ShowSchema),
    (&["thread list"], ThreadListSchema),
    (&["schemas"], SchemasListSchema),
    (&["review show"], ReviewShowSchema),
    (&["review sign"], ReviewSignSchema),
    (&["review next"], ReviewNextSchema),
    (&["review health"], ReviewHealthSchema),
    (&["retro"], RetroSchema),
    (&["discuss open", "discuss append", "discuss resolve", "discuss show"], DiscussionEnvelopeSchema),
    (&["discuss list"], DiscussionListSchema),
    (&["query --attribution"], BlameSchema),
    (&["transaction commit"], TransactionCommitSchema),
    (&["export git"], ExportGitSchema),
    (&["import git"], ImportGitSchema),
    (&["sync git"], SyncGitSchema),
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
    (&["agent task create", "agent task show", "agent task update"], AgentTaskEnvelopeSchema),
    (&["agent task list"], AgentTaskListSchema),
    (&["agent fanout plan", "agent fanout start"], AgentFanoutSchema),
    (&["auth logout"], AuthLogoutSchema),
    (&["auth status"], AuthStatusSchema),
    (&["auth create-service-token"], AuthCreateServiceTokenSchema),
    (&["session start", "session end", "session show"], SessionEnvelopeSchema),
    (&["session segment"], SessionSegmentEnvelopeSchema),
    (&["session list"], SessionListSchema),
    (&["support grant"], SupportGrantSchema),
    (&["support list"], SupportAccessListSchema),
    (&["support revoke"], SupportRevokeSchema),
    (&["git-overlay"], GitOverlayGuideSchema),
    (&["watch"], WatchLineSchema),
    (&["integration list", "integration doctor"], IntegrationStatusListSchema),
    (&["try"], TrySchema),
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
    let verb = verb.trim();
    if !schema_verbs().contains(&verb) {
        return None;
    }
    let mut schema = schema_for_registered_verb(verb)
        .or_else(|| schema_for_report_contract_verb(verb))
        .or_else(|| {
            opaque_schema_verbs()
                .contains(&verb)
                .then(|| serde_json::to_value(schema_for!(GenericJsonObjectSchema)).ok())
                .flatten()
        })?;
    add_op_id_replay_fields_if_supported(verb, &mut schema);
    add_json_discriminator_if_advertised(verb, &mut schema);
    Some(schema)
}

fn schema_for_report_contract_verb(verb: &str) -> Option<Value> {
    match verb {
        verb if verb == QueryReport::CONTRACT.schema_name => Some((QueryReport::CONTRACT.schema)()),
        verb if verb == DiffReport::CONTRACT.schema_name => Some((DiffReport::CONTRACT.schema)()),
        verb if verb == FsckReport::CONTRACT.schema_name => Some((FsckReport::CONTRACT.schema)()),
        verb if verb == StatusReport::CONTRACT.schema_name => {
            Some((StatusReport::CONTRACT.schema)())
        }
        verb if verb == VerifyReport::CONTRACT.schema_name => {
            Some((VerifyReport::CONTRACT.schema)())
        }
        _ => None,
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
    let mut discriminators = command_catalog::command_json_discriminators_for_schema_verb(verb);
    if schema.get("anyOf").is_some() {
        for discriminator in command_catalog::command_json_discriminators()
            .into_iter()
            .filter(|discriminator| {
                discriminator.display == verb && discriminator.schema_verb.as_deref() != Some(verb)
            })
        {
            discriminators.push(discriminator);
        }
    }
    discriminators.sort_by(|left, right| {
        (&left.field, &left.value, &left.display).cmp(&(&right.field, &right.value, &right.display))
    });
    discriminators.dedup_by(|left, right| left.field == right.field && left.value == right.value);

    if discriminators.is_empty() {
        return;
    };

    if add_json_discriminators_to_union_branches(verb, schema, &discriminators) {
        return;
    }

    let field = discriminators[0].field.as_str();
    let values = discriminators
        .iter()
        .filter(|discriminator| discriminator.field == field)
        .map(|discriminator| discriminator.value.as_str())
        .collect::<Vec<_>>();
    add_json_discriminator_to_schema_object(schema, field, &values);
}

fn add_json_discriminators_to_union_branches(
    verb: &str,
    schema: &mut Value,
    discriminators: &[command_catalog::CommandJsonDiscriminator],
) -> bool {
    let Some(branches) = schema
        .get_mut("anyOf")
        .and_then(|value| value.as_array_mut())
    else {
        return false;
    };

    let mut injected = 0usize;
    for branch in branches {
        let Some(branch_ref) = branch
            .get("$ref")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Some(discriminator) = discriminator_for_union_branch(verb, &branch_ref, discriminators)
        else {
            continue;
        };
        let original_branch = branch.clone();
        let mut discriminator_schema = serde_json::json!({ "type": "object" });
        add_json_discriminator_to_schema_object(
            &mut discriminator_schema,
            &discriminator.field,
            &[&discriminator.value],
        );
        *branch = serde_json::json!({
            "allOf": [original_branch, discriminator_schema],
        });
        injected += 1;
    }

    injected > 0
}

fn discriminator_for_union_branch<'a>(
    verb: &str,
    branch_ref: &str,
    discriminators: &'a [command_catalog::CommandJsonDiscriminator],
) -> Option<&'a command_catalog::CommandJsonDiscriminator> {
    if discriminators.len() == 1 {
        return discriminators.first();
    }

    let def_name = schema_ref_name(branch_ref)?;
    if verb == "inspect" {
        let value = match def_name {
            "ShowSchema" => "inspect_state",
            "ThreadShowSchema" => "thread_show",
            _ => return None,
        };
        return discriminators
            .iter()
            .find(|discriminator| discriminator.value == value);
    }

    None
}

fn schema_ref_name(reference: &str) -> Option<&str> {
    reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))
}

fn add_json_discriminator_to_schema_object(schema: &mut Value, field: &str, values: &[&str]) {
    let enum_values = values
        .iter()
        .map(|value| Value::String((*value).to_string()))
        .collect::<Vec<_>>();

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
        field.to_string(),
        serde_json::json!({
            "type": "string",
            "enum": enum_values,
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
        .any(|required_field| required_field.as_str() == Some(field))
    {
        required.push(Value::String(field.to_string()));
    }
}

fn schema_verb_supports_op_id(verb: &str) -> bool {
    command_catalog::command_runtime_contract_for_schema_verb(verb)
        .is_some_and(|contract| contract.supports_op_id)
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
        "Run `heddle schemas` to list schema-backed verbs, or inspect the command catalog with `heddle help --output json`.".to_string()
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
        "heddle help --output json".to_string(),
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
    let bare = command_catalog::schema_verb_without_flags(normalized);
    if bare.is_empty() {
        return Vec::new();
    }

    known_verbs
        .iter()
        .copied()
        .filter(|known| {
            known.starts_with(normalized)
                || command_catalog::schema_verb_without_flags(known).starts_with(&bare)
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
    let bare = command_catalog::schema_verb_without_flags(normalized);
    let prefix = format!("{bare} ");
    known_verbs
        .iter()
        .copied()
        .filter(|known| {
            *known != normalized
                && (*known == bare
                    || known.starts_with(&prefix)
                    || command_catalog::schema_verb_without_flags(known) == bare)
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
pub struct AuthLogoutSchema {
    pub output_kind: String,
    pub server: String,
    pub removed: bool,
    pub device_identity_removed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuthStatusSchema {
    pub output_kind: String,
    pub server: String,
    pub authenticated: bool,
    pub subject: Option<String>,
    pub credential_id: Option<String>,
    pub expires_at: Option<String>,
    pub recommended_action: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuthCreateServiceTokenSchema {
    pub output_kind: String,
    pub name: String,
    pub namespace: String,
    pub scope: String,
    pub token: String,
    /// Path to the private-key PEM written with mode 0600.
    pub private_key_path: String,
    /// Present only when `--show-secrets` was passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_pem: Option<String>,
    pub expires_in_days: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SupportAccessSchema {
    pub id: String,
    pub operator_email: String,
    pub namespace_path: String,
    pub repo_path: String,
    pub role: String,
    pub granted_by: String,
    pub granted_at: u64,
    pub expires_at: u64,
    pub revoked_at: u64,
    pub revoked_by: String,
    pub reason: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SupportGrantSchema {
    pub output_kind: String,
    pub id: String,
    pub operator_email: String,
    pub namespace_path: String,
    pub repo_path: String,
    pub role: String,
    pub granted_by: String,
    pub granted_at: u64,
    pub expires_at: u64,
    pub revoked_at: u64,
    pub revoked_by: String,
    pub reason: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SupportAccessListSchema {
    pub output_kind: String,
    pub grants: Vec<SupportAccessSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SupportRevokeSchema {
    pub output_kind: String,
    pub id: String,
    pub revoked: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IntegrationStatusListSchema(pub Vec<IntegrationStatusSchema>);

#[derive(Debug, Serialize, JsonSchema)]
pub struct IntegrationStatusSchema {
    pub harness: String,
    pub scope: String,
    pub method: String,
    pub status: String,
    pub healthy: bool,
    pub paths: Vec<String>,
    pub capabilities: Vec<String>,
    pub capability_paths: Vec<String>,
    pub path_mode: String,
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
pub struct ResolveSchema {
    pub output_kind: String,
    pub message: Option<String>,
    pub resolved: Option<Vec<String>>,
    pub remaining: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub continued: Option<bool>,
    pub continuation_status: Option<String>,
    pub continuation_message: Option<String>,
    pub next_action: Option<String>,
    pub recommended_action: Option<String>,
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
    pub agent_tasks: Vec<RetroAgentTaskEntrySchema>,
    pub timeline_steps: Vec<RetroTimelineStepEntrySchema>,
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
pub struct RetroAgentTaskEntrySchema {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub target_thread: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub coordination_discussion_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RetroTimelineStepEntrySchema {
    pub thread: String,
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<String>,
    pub changed: Option<bool>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub before_state: Option<String>,
    pub after_state: Option<String>,
    pub capture_state: Option<String>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
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
    pub change_id: Option<String>,
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
    pub task_assignment_id: Option<String>,
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
pub struct SwitchCheckoutSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub action: Option<String>,
    pub name: Option<String>,
    pub message: String,
    pub thread: Option<ThreadSummarySchema>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    pub target: Option<String>,
    pub intent: Option<String>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplateSchema>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MergePreviewSchema {
    pub output_kind: Option<String>,
    pub status: Option<String>,
    pub action: Option<String>,
    pub message: Option<String>,
    pub would_merge: bool,
    pub applied: bool,
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
    pub merge_relation: Option<String>,
    pub conflict_count: Option<usize>,
    pub thread_health: Option<String>,
    pub diff: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadySchema {
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
    pub captured: bool,
    pub captured_state: Option<String>,
    pub thread_state: Option<String>,
    pub readiness: ReadyReadinessSchema,
    pub report: Value,
    #[serde(rename = "verification")]
    pub verification: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadyReadinessSchema {
    pub status: String,
    pub captured: bool,
    pub captured_state: Option<String>,
    pub checks: ReadyChecksSchema,
    pub integration: String,
    pub freshness: String,
    pub merge_type: String,
    pub changed_path_count: usize,
    pub changed_paths: Vec<String>,
    pub conflict_count: usize,
    pub conflicts: Vec<String>,
    pub impact: String,
    pub impact_categories: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadyChecksSchema {
    pub status: String,
    pub reason: String,
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
    pub performed_steps: Vec<String>,
    pub skipped_steps: Vec<String>,
    pub merge_state: Option<String>,
    pub chosen_path: String,
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
    pub fskit_readiness: Option<FsKitReadinessSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FsKitReadinessSchema {
    pub state: String,
    pub backend: String,
    pub action: String,
    pub settings_url: Option<String>,
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
pub struct ThreadMarkerListSchema {
    pub output_kind: String,
    pub markers: Vec<ThreadMarkerEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadMarkerEntrySchema {
    pub name: String,
    pub change_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadMarkerOpSchema {
    pub output_kind: String,
    pub name: Option<String>,
    pub change_id: Option<String>,
    pub deleted: Option<Vec<ThreadMarkerEntrySchema>>,
    pub count: Option<usize>,
    pub message: String,
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
    pub task_assignment_id: Option<String>,
    pub task_summary: Option<ThreadTaskSummarySchema>,
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
    // Runtime `ThreadSummary.recommended_action` is a `String` serialized
    // through `serialize_empty_action_as_null`, so the wire value is
    // `string | null` (HeddleCo/heddle#645 presence contract).
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub git_branch_tip: Option<String>,
    pub history_imported: bool,
    pub auto: bool,
    pub shared_target_dir: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadTaskSummarySchema {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub target_thread: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub coordination_discussion_id: Option<String>,
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
    pub output_kind: String,
    pub actor: ActorEntrySchema,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorListSchema {
    pub output_kind: String,
    pub actors: Vec<ActorEntrySchema>,
    pub active_only: bool,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorDoneSchema {
    pub output_kind: String,
    pub session_id: String,
    pub status: String,
    pub thread: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action_template: Option<ActionTemplateSchema>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorExplainSchema {
    pub output_kind: String,
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
    pub token: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
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
    pub lease_id: String,
    pub actor_session_id: Option<String>,
    pub thread: String,
    pub anchor_state: Option<String>,
    pub anchor_root: Option<String>,
    pub task_assignment_id: Option<String>,
    pub status: String,
    pub path: Option<String>,
    pub heartbeat_at: String,
    pub lease_expires_at: String,
    pub liveness: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentTaskEnvelopeSchema {
    pub output_kind: String,
    pub task: AgentTaskSchema,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentTaskListSchema {
    pub output_kind: String,
    pub tasks: Vec<AgentTaskSchema>,
    pub thread: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentTaskSchema {
    pub schema_version: u32,
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub target_thread: String,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub parent_task_id: Option<String>,
    pub coordination_discussion_id: Option<String>,
    pub allow_offline: bool,
    pub delegated_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentFanoutSchema {
    pub output_kind: String,
    pub title: String,
    pub parent_thread: String,
    pub base_state: String,
    pub base_root: String,
    pub coordination_discussion_id: Option<String>,
    pub parent_task: Option<AgentTaskSchema>,
    pub lanes: Vec<AgentFanoutLaneSchema>,
    pub commands: Vec<AgentFanoutCommandSchema>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentFanoutLaneSchema {
    pub thread: String,
    pub path: String,
    pub title: String,
    pub task: Option<AgentTaskSchema>,
    pub session_id: Option<String>,
    pub status: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentFanoutCommandSchema {
    pub lane_thread: String,
    pub command: String,
    pub argv: Vec<String>,
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
    /// Full ref names this push wrote at the destination (sorted; empty
    /// for a no-op push). Present on the Git-overlay refs path; omitted
    /// on the native Heddle transport. Verify with `git ls-remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs_written: Option<Vec<String>>,
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
    // Required AND nullable (the `#[schemars(required)]` shorthand strips
    // the null variant from `Option<T>`, mis-declaring the wire contract —
    // HeddleCo/heddle#645 conformance): push always emits these fields,
    // serializing null for the no-action case.
    pub next_action: NullableStringSchema,
    pub next_action_template: NullableActionTemplateSchema,
    pub recommended_action: NullableStringSchema,
    pub recommended_action_template: NullableActionTemplateSchema,
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

// ---- verify ---------------------------------------------------------------

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
    pub machine_contract_coverage: MachineContractCoverageSchema,
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
    /// Whether an agent may replace placeholders in `argv_template`.
    ///
    /// When `agent_may_fill` is false, treat `action` and `argv_template` as
    /// display-only: do not substitute `<name>`/`<url>` placeholders. Surface
    /// the template to a human or discard it. Substituting and running it will
    /// pass literal `<name>` to Heddle and fail.
    pub agent_may_fill: bool,
}

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
    pub collapsed: Option<CollapsedEntrySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CollapsedEntrySchema {
    pub expandable: bool,
    pub source_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineLogSchema {
    pub output_kind: String,
    pub status: String,
    pub repository_capability: String,
    pub storage_model: String,
    pub thread: String,
    pub cursor: TimelineCursorSchema,
    pub branches: Vec<TimelineBranchSchema>,
    pub steps: Vec<TimelineStepSchema>,
    pub active_branch_path: Vec<String>,
    pub actions: TimelineActionsSchema,
    pub recovery: Option<TimelineRecoverySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineCursorSchema {
    pub branch_id: Option<String>,
    pub step_id: Option<String>,
    pub state: Option<String>,
    pub state_full: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineBranchSchema {
    pub branch_id: String,
    pub parent_branch_id: Option<String>,
    pub forked_from_step_id: Option<String>,
    pub forked_from_state: Option<String>,
    pub reason: Option<String>,
    pub created_at_ms: Option<i64>,
    pub step_ids: Vec<String>,
    pub is_active: bool,
    pub is_on_active_path: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineStepSchema {
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub native: Option<TimelineNativeSchema>,
    pub tool_name: Option<String>,
    pub status: Option<String>,
    pub changed: Option<bool>,
    pub touched_paths: Vec<String>,
    pub labels: Vec<String>,
    pub before_state: Option<String>,
    pub after_state: Option<String>,
    pub capture_state: Option<String>,
    pub cursor_state: Option<String>,
    pub cursor_state_full: Option<String>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub capture_oplog_batch_id: Option<u64>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub operation_ids: Vec<String>,
    pub is_current: bool,
    pub is_on_active_branch_path: bool,
    pub can_seek: bool,
    pub can_fork: bool,
    pub can_reset: bool,
    pub can_materialize: bool,
    pub has_boundary_warning: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineNativeSchema {
    pub harness: String,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_call_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineActionsSchema {
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineRecoverySchema {
    pub status: String,
    pub branch_id: String,
    pub from_step_id: Option<String>,
    pub to_step_id: Option<String>,
    pub from_state: String,
    pub to_state: String,
    pub reason: String,
    pub moved_at_ms: i64,
    pub checkout_state: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineStatusSchema {
    pub output_kind: String,
    pub status: String,
    pub thread: String,
    pub cursor_branch_id: Option<String>,
    pub cursor_step_id: Option<String>,
    pub cursor_state: Option<String>,
    pub current_step: Option<TimelineStatusStepSchema>,
    pub active_branch_path: Vec<String>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub branch_count: usize,
    pub step_count: usize,
    pub recovery: Option<TimelineStatusRecoverySchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineStatusStepSchema {
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<String>,
    pub changed: Option<bool>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub labels: Vec<String>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub can_seek: bool,
    pub can_fork: bool,
    pub can_reset: bool,
    pub can_materialize: bool,
    pub has_boundary_warning: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineStatusRecoverySchema {
    pub status: String,
    pub branch_id: String,
    pub from_step_id: Option<String>,
    pub to_step_id: Option<String>,
    pub from_state: String,
    pub to_state: String,
    pub reason: String,
    pub moved_at_ms: i64,
    pub checkout_state: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineRecordingSchema {
    pub output_kind: String,
    pub status: String,
    pub action: String,
    pub thread: String,
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub operation_id: String,
    pub before_state: Option<String>,
    pub after_state: Option<String>,
    pub changed: Option<bool>,
    pub tool_status: Option<String>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub branch_count: usize,
    pub step_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TimelineActionSchema {
    pub output_kind: String,
    pub status: String,
    pub action: String,
    pub thread: String,
    pub branch_id: Option<String>,
    pub parent_branch_id: Option<String>,
    pub from_step_id: Option<String>,
    pub cursor_branch_id: Option<String>,
    pub cursor_step_id: Option<String>,
    pub operation_id: Option<String>,
    pub recovered_operation_id: Option<String>,
    pub materialized: Option<bool>,
    pub materialization_status: Option<String>,
    pub recovery_status: Option<String>,
    pub blocker_count: usize,
    pub branch_count: usize,
    pub step_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExpandSchema {
    pub output_kind: String,
    pub status: String,
    pub requested: String,
    pub collapsed: ExpandedCollapseSchema,
    pub captures: Vec<ExpandedCaptureSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExpandedCollapseSchema {
    pub change_id: String,
    pub change_id_full: String,
    pub git_commit: Option<String>,
    pub thread: Option<String>,
    pub source_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExpandedCaptureSchema {
    pub change_id: String,
    pub change_id_full: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub principal: String,
    pub agent: Option<String>,
    pub confidence: Option<f32>,
    pub created_at: String,
    pub parents: Vec<String>,
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
    pub output_kind: String,
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
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplateSchema>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AvailableGitRefSchema {
    pub name: String,
    pub git_commit: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
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

// ---- git projection ops -----------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExportedRefSchema {
    pub name: String,
    pub tip: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExportGitSchema {
    pub output_kind: Option<String>,
    pub states_exported: u64,
    pub commits_total: u64,
    pub threads_synced: u64,
    pub markers_synced: u64,
    pub branches: Vec<ExportedRefSchema>,
    pub tags: Vec<ExportedRefSchema>,
    pub destination: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ImportGitSchema {
    pub output_kind: Option<String>,
    pub status: String,
    pub action: Option<String>,
    pub summary: String,
    pub commits_imported: u64,
    pub states_created: u64,
    pub branches_synced: u64,
    pub tags_synced: u64,
    pub skipped_non_commit_refs: u64,
    pub lossy_entries: Vec<LossyImportEntrySchema>,
    pub already_in_sync: bool,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplateSchema>,
    pub recovery_commands: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LossyImportEntrySchema {
    pub path: String,
    pub action: String,
    pub reason: String,
    pub git_object: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SyncGitSchema {
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
pub struct RevertSchema {
    pub output_kind: String,
    pub change_id: Option<String>,
    pub reverted_state: String,
    pub files_affected: Vec<String>,
    pub message: String,
}

// ---- git overlay diagnostics ---------------------------------------------

// ---- diagnose -------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiagnoseSchema {
    pub output_kind: Option<String>,
    pub repository: String,
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationStateSchema,
    pub operation: OpaqueObject,
    pub remote_tracking: OpaqueObject,
    pub thread: Option<Value>,
    pub state: Option<Value>,
    pub changes: Value,
    pub workspace: Value,
    pub health: Value,
    pub recommended_action: Option<String>,
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

    fn resolve_schema_ref<'a>(root: &'a Value, reference: &str) -> &'a Value {
        reference
            .strip_prefix("#/$defs/")
            .or_else(|| reference.strip_prefix("#/definitions/"))
            .and_then(|name| {
                root.get("$defs")
                    .or_else(|| root.get("definitions"))
                    .and_then(|defs| defs.get(name))
            })
            .unwrap_or_else(|| panic!("schema reference `{reference}` resolves"))
    }

    fn schema_declares_property(root: &Value, schema: &Value, property: &str) -> bool {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            return schema_declares_property(root, resolve_schema_ref(root, reference), property);
        }

        if schema
            .get("properties")
            .and_then(|properties| properties.get(property))
            .is_some()
        {
            return true;
        }

        for combinator in ["anyOf", "oneOf"] {
            if let Some(schemas) = schema.get(combinator).and_then(|value| value.as_array()) {
                return !schemas.is_empty()
                    && schemas
                        .iter()
                        .all(|schema| schema_declares_property(root, schema, property));
            }
        }

        schema
            .get("allOf")
            .and_then(|value| value.as_array())
            .is_some_and(|schemas| {
                schemas
                    .iter()
                    .any(|schema| schema_declares_property(root, schema, property))
            })
    }

    fn schema_allows_null(root: &Value, schema: &Value) -> bool {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            return schema_allows_null(root, resolve_schema_ref(root, reference));
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
            collect_string_enums(root, resolve_schema_ref(root, reference), values);
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

    fn collect_discriminator_values<'a>(
        root: &'a Value,
        schema: &'a Value,
        field: &str,
        values: &mut Vec<&'a str>,
    ) {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            collect_discriminator_values(root, resolve_schema_ref(root, reference), field, values);
            return;
        }

        if let Some(property) = schema
            .get("properties")
            .and_then(|properties| properties.get(field))
        {
            collect_string_enums(root, property, values);
        }

        for combinator in ["anyOf", "oneOf", "allOf"] {
            if let Some(schemas) = schema.get(combinator).and_then(|value| value.as_array()) {
                for schema in schemas {
                    collect_discriminator_values(root, schema, field, values);
                }
            }
        }
    }

    fn schema_requires_discriminator(root: &Value, schema: &Value, field: &str) -> bool {
        if let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) {
            return schema_requires_discriminator(root, resolve_schema_ref(root, reference), field);
        }

        if schema
            .get("properties")
            .and_then(|properties| properties.get(field))
            .is_some()
        {
            return schema
                .get("required")
                .and_then(|value| value.as_array())
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|required_field| required_field.as_str() == Some(field))
                });
        }

        for combinator in ["anyOf", "oneOf"] {
            if let Some(schemas) = schema.get(combinator).and_then(|value| value.as_array()) {
                return !schemas.is_empty()
                    && schemas
                        .iter()
                        .all(|schema| schema_requires_discriminator(root, schema, field));
            }
        }

        schema
            .get("allOf")
            .and_then(|value| value.as_array())
            .is_some_and(|schemas| {
                schemas
                    .iter()
                    .any(|schema| schema_requires_discriminator(root, schema, field))
            })
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
    /// `CleanSchema`, `DiffSchema`) never declares the field. The mirror
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
            let bare = schema_for_report_contract_verb(verb)
                .or_else(|| schema_for_registered_verb(verb))
                .unwrap_or_else(|| panic!("documented verb `{verb}` has no registered schema"));
            let declares = schema_declares_property(&bare, &bare, "output_kind");
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
            "`heddle help --output json` command schema verbs must match `heddle schemas` except for the cross-cutting JSON error envelope"
        );
    }

    #[cfg(not(feature = "git-overlay"))]
    #[test]
    fn native_only_schema_registry_excludes_git_overlay_verbs() {
        let catalog = command_catalog::build_command_catalog();
        for verb in [
            "import git",
            "export git",
            "sync git",
            "context reason git",
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
            "verification",
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
        for legacy in &["git_overlay_import_hint", "git_overlay_health"] {
            assert!(
                !properties.contains_key(*legacy),
                "status schema must expose verification, not legacy Git overlay sidecar '{legacy}'"
            );
        }
    }

    #[test]
    fn verify_schema_nests_repository_verification_state() {
        let schema = schema_for_verb("verify").expect("verify schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("verify schema has properties");
        assert!(
            properties.contains_key("verification"),
            "verify schema must expose nested verification state"
        );
        for flattened in ["verified", "status", "checks", "recommended_action"] {
            assert!(
                !properties.contains_key(flattened),
                "verify schema must not expose flattened verification property `{flattened}`"
            );
        }
    }

    #[test]
    fn action_template_agent_may_fill_schema_describes_false_semantics() {
        let schema = schema_for_verb("verify").expect("verify schema");
        let action_template = schema
            .get("$defs")
            .or_else(|| schema.get("definitions"))
            .and_then(|defs| {
                defs.get("ActionTemplate")
                    .or_else(|| defs.get("ActionTemplateSchema"))
            })
            .expect("verify schema includes ActionTemplateSchema definition");
        let description = property_schema(action_template, "agent_may_fill")
            .get("description")
            .and_then(Value::as_str)
            .expect("agent_may_fill schema description is present");

        assert!(
            description.contains("When `agent_may_fill` is false"),
            "agent_may_fill schema description must document false semantics: {description}"
        );
        assert!(
            description.contains("display-only"),
            "agent_may_fill schema description must warn agents not to execute display-only templates: {description}"
        );
        assert!(
            description.contains("do not substitute `<name>`/`<url>` placeholders"),
            "agent_may_fill schema description must prohibit placeholder substitution when false: {description}"
        );
    }

    /// HeddleCo/heddle#645 conformance: the action-field presence contract.
    ///
    /// `next_action` / `recommended_action` encode "no action needed" as
    /// `null` and "not applicable to this output shape" as an absent
    /// field — never as `""` (the runtime maps empty selections to `None`
    /// via `next_action::normalized_action` /
    /// `serialize_empty_action_as_null`, and the serialization walker in
    /// `validate_next_actions_at_path` rejects any empty string that
    /// slips past). At the schema level that means: wherever one of these
    /// properties is *required*, its schema must allow `null` — a
    /// non-nullable required action field would force emitters to leak
    /// `""` for the no-action case.
    #[test]
    fn action_fields_follow_presence_contract_in_every_schema() {
        fn walk(root: &Value, schema: &Value, verb: &str, path: &str) {
            match schema {
                Value::Object(object) => {
                    if let Some(properties) = object.get("properties").and_then(|p| p.as_object()) {
                        let required: Vec<&str> = object
                            .get("required")
                            .and_then(|value| value.as_array())
                            .map(|fields| {
                                fields.iter().filter_map(|field| field.as_str()).collect()
                            })
                            .unwrap_or_default();
                        for (name, child) in properties {
                            if matches!(name.as_str(), "next_action" | "recommended_action")
                                && required.contains(&name.as_str())
                            {
                                assert!(
                                    schema_allows_null(root, child),
                                    "`{verb}` schema requires `{path}.{name}` without allowing \
                                     null; the action contract is null = no action, absent = \
                                     not applicable, never \"\": {child}"
                                );
                            }
                        }
                    }
                    for (key, child) in object {
                        walk(root, child, verb, &format!("{path}.{key}"));
                    }
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        walk(root, child, verb, &format!("{path}[{index}]"));
                    }
                }
                _ => {}
            }
        }

        for verb in schema_verbs() {
            let schema =
                schema_for_verb(verb).unwrap_or_else(|| panic!("schema registered for `{verb}`"));
            walk(&schema, &schema, verb, "$");
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
    fn status_agent_context_fields_are_omittable() {
        let schema = schema_for_verb("status").expect("status schema");
        let required = required_fields(&schema);
        for field in [
            "path",
            "execution_path",
            "session_id",
            "heddle_session_id",
            "actor",
            "harness",
            "thinking_level",
            "usage_summary",
            "last_progress_at",
            "report_flush_state",
            "attach_reason",
            "target_thread",
            "parent_thread",
            "task",
        ] {
            assert!(
                !required.contains(&field),
                "status `{field}` is omitted when no agent/materialized context is recorded: {schema}"
            );
        }
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
    fn ready_schema_requires_stable_operator_and_readiness_fields() {
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
        assert!(
            properties.contains_key("readiness"),
            "ready schema should document the stable readiness summary"
        );
        assert!(
            properties.contains_key("verification"),
            "ready schema should document the repository verification proof"
        );

        let required = required_fields(&schema);
        for stable_field in ["blockers", "warnings", "readiness", "verification"] {
            assert!(
                required.contains(&stable_field),
                "ready schema must require `{stable_field}` because ready JSON always emits the stable field set: {schema}"
            );
        }
        assert!(
            properties.contains_key("captured_state"),
            "ready schema should document captured_state even though schemars models nullable Option fields as optional"
        );
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
        use std::collections::{BTreeMap, BTreeSet};

        for schema_verb in schema_verbs() {
            let mut discriminators =
                command_catalog::command_json_discriminators_for_schema_verb(schema_verb);
            if discriminators.is_empty() {
                continue;
            };
            let schema =
                schema_for_verb(schema_verb).unwrap_or_else(|| panic!("{schema_verb} schema"));
            if schema.get("anyOf").is_some() {
                // A union schema published under this verb covers every schema
                // verb its catalog entry documents — the expected discriminator
                // set must include the siblings (e.g. inspect's union carries
                // the `thread show` branch's thread_show).
                for sibling in command_catalog::sibling_documented_schema_verbs(schema_verb) {
                    discriminators.extend(
                        command_catalog::command_json_discriminators_for_schema_verb(sibling),
                    );
                }
                for discriminator in command_catalog::command_json_discriminators()
                    .into_iter()
                    .filter(|discriminator| {
                        discriminator.display == *schema_verb
                            && discriminator.schema_verb.as_deref() != Some(schema_verb)
                    })
                {
                    discriminators.push(discriminator);
                }
            }

            let mut expected_by_field = BTreeMap::<String, BTreeSet<String>>::new();
            for discriminator in discriminators {
                expected_by_field
                    .entry(discriminator.field)
                    .or_default()
                    .insert(discriminator.value);
            }

            for (field, expected) in expected_by_field {
                let mut actual = Vec::new();
                collect_discriminator_values(&schema, &schema, &field, &mut actual);
                let actual = actual
                    .into_iter()
                    .map(str::to_string)
                    .collect::<BTreeSet<_>>();
                assert_eq!(
                    actual, expected,
                    "{schema_verb} schema must narrow `{field}` to every catalog-advertised value"
                );
                assert!(
                    schema_requires_discriminator(&schema, &schema, &field),
                    "{schema_verb} schema must require discriminator field `{field}`"
                );
            }
        }
    }

    #[test]
    fn oss_recovery_surfaces_do_not_use_opaque_generic_schema() {
        for verb in [
            "fsck",
            "resolve",
            "retro",
            "discuss open",
            "discuss append",
            "discuss resolve",
            "discuss list",
            "discuss show",
            "query",
            "query --attribution",
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
            "op-id schema coverage test should exercise multiple verbs"
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
