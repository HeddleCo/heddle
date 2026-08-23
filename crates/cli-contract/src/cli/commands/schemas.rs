// SPDX-License-Identifier: Apache-2.0
//! JSON Schema registry for `--output json` verb payloads.
//!
//! This module owns the JSON Schema registry for `--output json`
//! verbs. Schema *membership* comes from the active command catalog.
//! `init` registers the real [`InitOutput`] type so the published
//! schema cannot drift from the serializer. Remaining verbs still use
//! schemars-derived mirror structs to avoid threading `JsonSchema`
//! through every workspace output type. `heddle doctor schemas`
//! validates documented samples against the registered schemas.
//!
//! See [`super::doctor_schemas`] for the drift checker.

use std::{collections::BTreeMap, sync::OnceLock};

use repo::{RepositoryMaintenanceRunReport, RepositoryPerformanceInspectionReport};
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::Value;
use verbs::{
    DiffReport, FsckReport, QueryReport, RemoteListReport, ResolveReport, StatusReport,
    ThreadMoveOutput, UndoListReport, VerifyReport, remote::RemoteInfo,
};

use super::command_catalog;
use super::doctor_docs::DocsReport;
use super::doctor_schemas::SchemaReport;
use super::init_output::InitOutput;
use super::wire::agent::{
    ActorDoneOutput, ActorExplainDetectedOutput, ActorListOutput, ActorSingleOutput,
    AgentFanoutOutput, AgentReservationEnvelope, AgentReservationListOutput, AgentTaskEnvelope,
    AgentTaskListOutput, SegmentEnvelope, SessionEnvelope, SessionListOutput,
};
use super::wire::auth::{
    AuthLogoutOutput, AuthStatusOutput, AuthTrustOutput, IdentityOutput, ServiceTokenOutput,
    WhoamiOutput,
};
use super::wire::thread::{
    ApprovalOutput, ApprovalRevokeOutput, EligibilityOutput, ThreadAbsorbOutput,
    ThreadCleanupOutput, ThreadOpOutput, ThreadRecordOutput, ThreadResolveOutput,
};
use super::wire::{
    AdoptOutput, BlameOutput, CloneOutput, CommitOutput, DiscussionListOutput,
    DiscussionShowOutput, DiscussionWriteOutput, ExpandOutput, ExportGitOutput, ImportGitOutput,
    IntegrationStatusOutput, LandOutput, LogOutput, MarkerBulkDeleteOutput, MarkerListOutput,
    MarkerOpOutput, MultiLandOutput, OperatorCommandOutput, PullOutput, PushOutput, ReadyOutput,
    ReflogOutput, RemoteMutationOutput, RepackOutput, RevertOutput, ReviewHealthOutput,
    ReviewNextOutput, ReviewShowOutput, ReviewSignOutput, ShowOutput, SnapshotOutput,
    SyncGitOutput, SyncOutput, ThreadCaptureOutput, ThreadCurrentOutput, ThreadListOutput,
    ThreadShowOutput, TimelineActionOutput, TimelineLogOutput, TimelineRecordingOutput,
    TimelineStatusOutput, UndoRedoOutput, WatchLineOutput,
};
use crate::cli::INIT_VERB;

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
        ResolveReport::CONTRACT.schema_name,
        DiffReport::CONTRACT.schema_name,
        FsckReport::CONTRACT.schema_name,
        StatusReport::CONTRACT.schema_name,
        VerifyReport::CONTRACT.schema_name,
    ]
}

schema_registry! {
    (&["maintenance fsck repair git"], FsckReport),
    (&[INIT_VERB], InitOutput),
    (&["adopt"], AdoptOutput),
    (&["capture"], SnapshotOutput),
    (&["commit"], CommitOutput),
    (&["undo", "undo --redo", "undo --recover"], UndoRedoOutput),
    (&["undo --list"], UndoListReport),
    (&["ready"], ReadyOutput),
    (&["land"], LandOutput),
    (&["land --threads"], MultiLandOutput),
    (&["sync"], SyncOutput),
    (&["continue", "abort"], OperatorCommandOutput),
    (&["start"], ThreadOpOutput),
    (&["thread create", "thread switch", "thread rename"], ThreadOpOutput),
    (&["thread current"], ThreadCurrentOutput),
    (&["thread captures"], Vec<ThreadCaptureOutput>),
    (&["thread refresh", "thread drop"], ThreadRecordOutput),
    (&["thread promote"], ThreadRecordOutput),
    (&["thread move"], ThreadMoveOutput),
    (&["thread absorb"], ThreadAbsorbOutput),
    (&["thread resolve"], ThreadResolveOutput),
    (&["thread approve"], ApprovalOutput),
    (&["thread approvals"], Vec<ApprovalOutput>),
    (&["thread revoke-approval"], ApprovalRevokeOutput),
    (&["thread check-merge"], EligibilityOutput),
    (&["thread cleanup"], ThreadCleanupOutput),
    (&["thread marker list"], MarkerListOutput),
    (&["thread marker create", "thread marker show"], MarkerOpOutput),
    (&["thread marker delete"], MarkerBulkDeleteOutput),
    (&["thread show"], ThreadShowOutput),
    (&["clone"], CloneOutput),
    (&["remote list"], RemoteListReport),
    (&["remote show"], RemoteInfo),
    (&["remote add", "remote remove", "remote set-default"], RemoteMutationOutput),
    (&["pull"], PullOutput),
    (&["push"], PushOutput),
    (&["thread expand"], ExpandOutput),
    (&["log"], LogOutput),
    (&["log --reflog"], ReflogOutput),
    (&["log --timeline"], TimelineLogOutput),
    (&["agent timeline status"], TimelineStatusOutput),
    (&["agent timeline record-start", "agent timeline record-finish"], TimelineRecordingOutput),
    (&["agent timeline fork", "agent timeline reset", "agent timeline recover"], TimelineActionOutput),
    (&["show"], ShowOutput),
    (&["thread list"], ThreadListOutput),
    (&["review show"], ReviewShowOutput),
    (&["review sign"], ReviewSignOutput),
    (&["review next"], ReviewNextOutput),
    (&["review health"], ReviewHealthOutput),
    (&["discuss open", "discuss append", "discuss resolve", "discuss reopen"], DiscussionWriteOutput),
    (&["discuss show"], DiscussionShowOutput),
    (&["discuss list"], DiscussionListOutput),
    (&["query --attribution"], BlameOutput),
    (&["bridge git export"], ExportGitOutput),
    (&["bridge git import"], ImportGitOutput),
    (&["sync git"], SyncGitOutput),
    (&["revert"], RevertOutput),
    (&["doctor"], DoctorSchema),
    (&["doctor docs"], DocsReport),
    (&["doctor schemas"], SchemaReport),
    (&["agent presence show"], ActorSingleOutput),
    (&["agent presence list"], ActorListOutput),
    (&["agent presence complete"], ActorDoneOutput),
    (&["agent presence explain"], ActorExplainDetectedOutput),
    (&["agent reserve", "agent heartbeat", "agent release"], AgentReservationEnvelope),
    (&["agent capture"], SnapshotOutput),
    (&["agent ready"], ReadyOutput),
    (&["agent list"], AgentReservationListOutput),
    (&["agent task create", "agent task show", "agent task update"], AgentTaskEnvelope),
    (&["agent task list"], AgentTaskListOutput),
    (&["agent fanout plan", "agent fanout start"], AgentFanoutOutput),
    (&["auth logout"], AuthLogoutOutput),
    (&["auth status"], AuthStatusOutput),
    (&["auth trust show", "auth trust replace"], AuthTrustOutput),
    (&["whoami"], WhoamiOutput),
    (&["auth create-service-token"], ServiceTokenOutput),
    (&["identity ensure", "identity claim-link"], IdentityOutput),
    (&["agent provenance begin", "agent provenance end", "agent provenance show"], SessionEnvelope),
    (&["agent provenance segment"], SegmentEnvelope),
    (&["agent provenance list"], SessionListOutput),
    (&["watch"], WatchLineOutput),
    (&["integration list", "integration doctor"], Vec<IntegrationStatusOutput>),
    (&["maintenance inspect"], MaintenanceInspectWire),
    (&["maintenance refresh"], MaintenanceRefreshWire),
    (&["maintenance repack"], RepackOutput),
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
    stabilize_land_output_shapes(verb, &mut schema);
    Some(schema)
}

fn require_object_fields(schema: &mut Value, fields: &[&str]) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let required = object
        .entry("required".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(required) = required.as_array_mut() else {
        return;
    };
    for field in fields {
        if !required.iter().any(|value| value.as_str() == Some(field)) {
            required.push(Value::String((*field).to_string()));
        }
    }
}

fn stabilize_land_output_shapes(verb: &str, schema: &mut Value) {
    match verb {
        "land" => require_object_fields(schema, &["siblings_restacked", "siblings_restack_failed"]),
        "land --threads" => {
            require_object_fields(
                schema,
                &[
                    "stopped_at",
                    "git_head",
                    "recommended_action",
                    "verification",
                ],
            );
            if let Some(peer) = schema
                .get_mut("$defs")
                .and_then(Value::as_object_mut)
                .and_then(|defs| defs.get_mut("LandBatchPeerSchema"))
            {
                require_object_fields(
                    peer,
                    &[
                        "siblings_restacked",
                        "siblings_restack_failed",
                        "blockers",
                        "warnings",
                        "recovery_commands",
                    ],
                );
            }
        }
        _ => {}
    }
}

fn schema_for_report_contract_verb(verb: &str) -> Option<Value> {
    match verb {
        verb if verb == QueryReport::CONTRACT.schema_name => Some((QueryReport::CONTRACT.schema)()),
        verb if verb == ResolveReport::CONTRACT.schema_name => {
            Some((ResolveReport::CONTRACT.schema)())
        }
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

// ---------------------------------------------------------------------------
// Mirror types
// ---------------------------------------------------------------------------
//
// Unmigrated verbs still use a mirror struct: serde attributes match
// the real serializer, and `schemars` emits the JSON Schema. `init`
// registers the real output type instead — do not add a mirror for it.
// When a remaining mirror's real output struct changes, update the
// mirror here and `docs/json-schemas.md`.

// ---- shared sub-types ------------------------------------------------------
//
// Variants here are referenced only through the schemars derive,
// which the dead-code lint can't see. The annotation keeps the
// surface honest without polluting downstream warnings.

#[derive(Debug, Serialize, JsonSchema)]
pub struct GenericJsonObjectSchema {
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
}

/// Wire envelope for `maintenance inspect`: `output_kind` beside the real
/// [`RepositoryPerformanceInspectionReport`] payload (InitOutput precedent).
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "MaintenanceInspectSchema")]
pub struct MaintenanceInspectWire {
    pub output_kind: String,
    #[serde(flatten)]
    pub report: RepositoryPerformanceInspectionReport,
}

/// Wire envelope for `maintenance refresh`: `output_kind` beside the real
/// [`RepositoryMaintenanceRunReport`] payload.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "MaintenanceRefreshSchema")]
pub struct MaintenanceRefreshWire {
    pub output_kind: String,
    #[serde(flatten)]
    pub run: RepositoryMaintenanceRunReport,
}

// ---- core loop write/read helpers -----------------------------------------

/// Operation banner — kept opaque because the underlying
/// [`repo::RepositoryOperationStatus`] is a workspace type and its
/// shape is internal. `Value` here means "any JSON object or null".
type OpaqueObject = Option<Value>;

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

// ---- show -----------------------------------------------------------------

// ---- thread list ----------------------------------------------------------

// ---- review ---------------------------------------------------------------

// ---- command/schema introspection ----------------------------------------

// ---- git projection ops -----------------------------------------------------------

// ---- git overlay diagnostics ---------------------------------------------

// ---- doctor ---------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoctorSchema {
    pub output_kind: String,
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
    /// so every emitted payload already surfaces `output_kind`. That
    /// injection masks the fact that the Rust mirror struct (e.g.
    /// `DiffSchema`) never declares the field. The mirror
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
             emission (the catalog injection masks this at the emission layer, \
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
            "`heddle help --output json` command schema verbs must match the registered schema registry except for the cross-cutting JSON error envelope"
        );
    }

    #[cfg(not(feature = "git-overlay"))]
    #[test]
    fn native_only_schema_registry_excludes_git_overlay_verbs() {
        let catalog = command_catalog::build_command_catalog();
        for verb in [
            "bridge git import",
            "bridge git export",
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
        for stable_field in [
            "blockers",
            "warnings",
            "capture_status",
            "capture_reason",
            "readiness",
            "verification",
        ] {
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
    fn land_schema_requires_structured_blocker_details() {
        let schema = schema_for_verb("land").expect("land schema");
        let properties = schema
            .get("properties")
            .and_then(|properties| properties.as_object())
            .expect("land schema has properties");
        assert!(properties.contains_key("blocker_details"), "{schema}");
        assert!(
            required_fields(&schema).contains(&"blocker_details"),
            "land always emits the machine-readable blocker detail array: {schema}"
        );
    }

    #[test]
    fn land_batch_peer_primary_command_remains_optional() {
        let schema = schema_for_verb("land --threads").expect("land batch schema");
        let peer = schema
            .get("$defs")
            .and_then(Value::as_object)
            .and_then(|defs| defs.get("LandBatchPeerSchema"))
            .expect("land batch peer schema");
        assert!(
            property_schema(peer, "primary_command").is_object(),
            "peer schema must still describe primary_command: {peer}"
        );
        assert!(
            !required_fields(peer).contains(&"primary_command"),
            "successful peers omit None primary_command values: {peer}"
        );
    }

    #[test]
    fn push_schema_requires_stable_runtime_fields() {
        let schema = schema_for_verb("push").expect("push schema");
        // The registered type is the real single-struct envelope: both
        // transports serialize one object whose optional facts are omitted.
        // The envelope always emits the action fields too, but they are
        // `Option` on the registered struct, so schemars leaves them optional
        // (nullable properties) rather than required.
        for stable_field in [
            "next_action",
            "next_action_template",
            "recommended_action",
            "recommended_action_template",
        ] {
            let property = property_schema(&schema, stable_field);
            assert!(
                property.is_object(),
                "push must still describe `{stable_field}`: {property}"
            );
        }
        for stable_field in [
            "output_kind",
            "action",
            "status",
            "pushed",
            "changed",
            "success",
            "transport",
            "verification",
        ] {
            assert!(
                required_fields(&schema).contains(&stable_field),
                "push must require `{stable_field}`: {schema}"
            );
        }
        for conditional in [
            "remote",
            "push_scope",
            "ref_scope",
            "refs_written",
            "tags_included",
            "force",
            "thread",
            "state",
            "objects",
        ] {
            let property = property_schema(&schema, conditional);
            assert!(
                !required_fields(&schema).contains(&conditional),
                "`{conditional}` is emitted only when present and must stay optional: {property}"
            );
            assert!(
                property.is_object(),
                "push must still describe `{conditional}`: {property}"
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
            "maintenance fsck",
            "resolve",
            "discuss open",
            "discuss append",
            "discuss resolve",
            "discuss reopen",
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
