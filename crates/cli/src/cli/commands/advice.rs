// SPDX-License-Identifier: Apache-2.0
//! Typed refusal and recovery advice shared by command surfaces.

use std::{error::Error, fmt};

use heddle_core::status::next_action::{
    canonical_git_import_ref_command, canonical_git_repair_ref_preview_command,
};
use serde_json::{Map, Value};

pub(crate) const DIRTY_WORKTREE_CAPTURE_COMMAND: &str = "heddle capture -m \"...\"";
pub(crate) const GIT_OVERLAY_CHECKPOINT_COMMAND: &str = "heddle commit -m \"...\"";

#[derive(Debug, Clone)]
pub struct RecoveryAdvice {
    pub kind: &'static str,
    pub error: String,
    pub hint: String,
    pub unsafe_condition: String,
    pub would_change: String,
    pub preserved: String,
    pub primary_command: String,
    pub recovery_commands: Vec<String>,
    pub extra_json_fields: Map<String, Value>,
}

impl RecoveryAdvice {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn safety_refusal(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        already_preserved: impl Into<String>,
        primary_command: impl Into<String>,
        recovery_commands: Vec<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        let recovery_commands = if recovery_commands.is_empty() {
            vec![primary_command.clone()]
        } else {
            recovery_commands
        };
        Self {
            kind,
            error: error.into(),
            hint: hint.into(),
            unsafe_condition: unsafe_condition.into(),
            would_change: would_change.into(),
            preserved: already_preserved.into(),
            primary_command,
            recovery_commands,
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn dirty_worktree(
        action: &str,
        dirty_paths: Vec<String>,
        already_preserved: impl Into<String>,
    ) -> Self {
        let path_list = if dirty_paths.is_empty() {
            "uncommitted paths were detected".to_string()
        } else {
            dirty_paths
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let overflow = dirty_paths.len().saturating_sub(12);
        let unsafe_condition = if overflow == 0 {
            format!("unsaved worktree path(s): {path_list}")
        } else {
            format!("unsaved worktree path(s): {path_list}, and {overflow} more")
        };
        let primary_command = DIRTY_WORKTREE_CAPTURE_COMMAND.to_string();
        Self {
            kind: "dirty_worktree",
            error: format!("Capture worktree changes before {action}"),
            hint: format!(
                "Capture the intended work with `{DIRTY_WORKTREE_CAPTURE_COMMAND}`, then retry."
            ),
            unsafe_condition,
            would_change: format!(
                "{action} would write another tree into the worktree; saving first prevents those path changes from being overwritten"
            ),
            preserved: already_preserved.into(),
            primary_command,
            recovery_commands: dirty_worktree_recovery_commands(),
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn git_head_mismatch(
        action: &str,
        current_oid: impl Into<String>,
        expected_oid: impl Into<String>,
        branch: impl Into<String>,
        dirty_paths: Vec<String>,
    ) -> Self {
        let current_oid = current_oid.into();
        let expected_oid = expected_oid.into();
        let branch = branch.into();
        let primary_command = canonical_git_repair_ref_preview_command(Some("heddle"), &branch);
        let dirty_summary = if dirty_paths.is_empty() {
            "dirty paths: none".to_string()
        } else {
            format!(
                "dirty paths: {}",
                dirty_paths
                    .iter()
                    .take(12)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut recovery_commands = vec![primary_command.clone()];
        if !dirty_paths.is_empty() {
            recovery_commands.extend(dirty_worktree_recovery_commands());
        }
        Self {
            kind: "git_head_mismatch",
            error: format!("Refusing to {action}: Git HEAD is not at the expected checkpoint"),
            hint: format!("Inspect recovery with `{primary_command}`."),
            unsafe_condition: format!(
                "current Git OID {current_oid}, expected {expected_oid}; {dirty_summary}"
            ),
            would_change: "moving Git now could overwrite commits Heddle did not checkpoint"
                .to_string(),
            preserved: "Heddle state was left unchanged".to_string(),
            primary_command: primary_command.clone(),
            recovery_commands,
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn destructive_requires_force(
        action: &str,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        preview_command: impl Into<String>,
        force_command: impl Into<String>,
        already_preserved: impl Into<String>,
    ) -> Self {
        let preview_command = preview_command.into();
        let force_command = force_command.into();
        Self {
            kind: "destructive_requires_force",
            error: format!("Refusing to {action}: destructive action requires --force"),
            hint: format!(
                "Inspect with `{preview_command}`; rerun `{force_command}` only if the removals are intentional."
            ),
            unsafe_condition: unsafe_condition.into(),
            would_change: would_change.into(),
            preserved: already_preserved.into(),
            primary_command: preview_command.clone(),
            recovery_commands: vec![preview_command, force_command],
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn op_id_conflict(
        command: &str,
        dedup_scope: &str,
        incoming_argv: &[String],
        incoming_request_hash: [u8; 32],
        existing: Option<repo::operation_dedup::DedupConflictMetadata>,
    ) -> Self {
        let existing_status = existing
            .as_ref()
            .map(|entry| {
                if entry.pending {
                    "in_flight"
                } else {
                    "completed"
                }
            })
            .unwrap_or("unknown");
        let mut extra_json_fields = Map::new();
        let recorded_command = existing
            .as_ref()
            .map(|entry| entry.verb.as_str())
            .unwrap_or(command);
        extra_json_fields.insert(
            "recorded_command".to_string(),
            Value::String(recorded_command.to_string()),
        );
        extra_json_fields.insert(
            "incoming_command".to_string(),
            Value::String(command.to_string()),
        );
        extra_json_fields.insert(
            "dedup_scope".to_string(),
            Value::String(dedup_scope.to_string()),
        );
        extra_json_fields.insert(
            "incoming_argv".to_string(),
            Value::Array(incoming_argv.iter().cloned().map(Value::String).collect()),
        );
        extra_json_fields.insert(
            "incoming_request_hash".to_string(),
            Value::String(hex_hash(incoming_request_hash)),
        );
        extra_json_fields.insert(
            "recorded_status".to_string(),
            Value::String(existing_status.to_string()),
        );
        if let Some(existing) = existing {
            extra_json_fields.insert(
                "recorded_request_hash".to_string(),
                Value::String(hex_hash(existing.request_hash)),
            );
            extra_json_fields.insert(
                "recorded_created_at_secs".to_string(),
                Value::Number(existing.created_at_secs.into()),
            );
        }
        Self {
            kind: "op_id_conflict",
            error: "--op-id was already used with different arguments".to_string(),
            hint: format!(
                "Retry with the original arguments for this --op-id in scope `{dedup_scope}` or generate a fresh operation id."
            ),
            unsafe_condition: format!(
                "the same operation id maps to a different request body for `heddle {command}` in scope `{dedup_scope}`"
            ),
            would_change:
                "reusing it for different arguments would make idempotent replay ambiguous"
                    .to_string(),
            preserved: "no command body was executed for this retry".to_string(),
            primary_command: "heddle help --output json".to_string(),
            recovery_commands: vec!["heddle help --output json".to_string()],
            extra_json_fields,
        }
    }

    pub(crate) fn op_id_in_flight() -> Self {
        Self {
            kind: "op_id_in_flight",
            error: "--op-id is currently being executed by another process".to_string(),
            hint: "Retry after the in-flight command completes; successful retries replay the cached result.".to_string(),
            unsafe_condition: "another process owns this operation id reservation".to_string(),
            would_change: "running a second copy could duplicate a mutating operation".to_string(),
            preserved: "no command body was executed for this retry".to_string(),
            primary_command: "heddle status".to_string(),
            recovery_commands: vec!["heddle status".to_string()],
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn op_id_unsupported(command: &str) -> Self {
        Self {
            kind: "op_id_unsupported",
            error: format!("--op-id is not supported by `heddle {command}`"),
            hint: "Inspect op-id support with `heddle help --output json` and retry without --op-id for this command.".to_string(),
            unsafe_condition: "the command contract marks this command as not idempotent".to_string(),
            would_change: "silently accepting --op-id here would imply a replay guarantee this command does not provide".to_string(),
            preserved: "no command body was executed".to_string(),
            primary_command: "heddle help --output json".to_string(),
            recovery_commands: vec!["heddle help --output json".to_string()],
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn op_id_invalid(raw: &str, parse_error: impl fmt::Display) -> Self {
        Self {
            kind: "op_id_invalid",
            error: format!("invalid --op-id `{raw}`: {parse_error}"),
            hint: "Pass a UUID v4 operation id, or omit --op-id to run without replay protection."
                .to_string(),
            unsafe_condition: "--op-id does not parse as a UUID v4".to_string(),
            would_change:
                "accepting a malformed operation id would make replay and conflict detection ambiguous"
                    .to_string(),
            preserved: "no command body was executed".to_string(),
            primary_command: "heddle help --output json".to_string(),
            recovery_commands: vec!["heddle help --output json".to_string()],
            extra_json_fields: Map::new(),
        }
    }

    pub fn json_unsupported(command: &str) -> Self {
        Self {
            kind: "json_unsupported",
            error: format!("--output json is not supported by `heddle {command}`"),
            hint: "Inspect JSON-capable commands with `heddle help --output json` or rerun with `--output text`.".to_string(),
            unsafe_condition: "the command contract marks this command as text-only".to_string(),
            would_change: "emitting ad hoc JSON here would create a machine contract outside the command table".to_string(),
            preserved: "no command body was executed".to_string(),
            primary_command: "heddle help --output json".to_string(),
            recovery_commands: vec![
                "heddle help --output json".to_string(),
                format!("heddle {command} --output text"),
            ],
            extra_json_fields: Map::new(),
        }
    }

    pub fn json_compact_unsupported(command: &str) -> Self {
        Self {
            kind: "json_compact_unsupported",
            error: format!("--output json-compact is not supported by `heddle {command}`"),
            hint: "Use `--output json` for the full machine contract, or choose a command that exposes a compact decision surface.".to_string(),
            unsafe_condition: "the command has no compact decision-surface projection".to_string(),
            would_change: "falling back to the full JSON contract would leak non-decision-surface fields under json-compact".to_string(),
            preserved: "no command body was executed".to_string(),
            primary_command: format!("heddle {command} --output json"),
            recovery_commands: vec![
                format!("heddle {command} --output json"),
                "heddle help --output json".to_string(),
            ],
            extra_json_fields: Map::new(),
        }
    }

    pub(crate) fn machine_contract_drift(
        error: impl Into<String>,
        unsafe_condition: impl Into<String>,
    ) -> Self {
        Self::safety_refusal(
            "machine_contract_drift",
            error,
            "Inspect the schema contract with `heddle doctor schemas --output json`, then update the schema registry or documented samples.",
            unsafe_condition,
            "continuing to rely on this machine contract could make JSON callers parse stale or undocumented fields",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle doctor schemas --output json",
            vec!["heddle doctor schemas --output json".to_string()],
        )
    }

    pub(crate) fn stale_daemon_protocol(their_version: u32, our_version: u32) -> Self {
        Self::safety_refusal(
            "daemon_protocol_version_mismatch",
            format!("heddled daemon is older (v{their_version}) than this CLI (v{our_version})"),
            "Stop the stale daemon so Heddle can respawn it with the current protocol, then retry.",
            format!(
                "daemon protocol version {their_version} is older than CLI protocol version {our_version}"
            ),
            "continuing over a stale daemon protocol could misread daemon responses or leave mount state unclear",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle daemon stop",
            vec![
                "heddle daemon stop".to_string(),
                "heddle status".to_string(),
            ],
        )
    }

    pub(crate) fn git_import_metadata_required(map_path: &str, git_path: &str) -> Self {
        let command = format!("heddle import git --path {git_path}");
        Self::safety_refusal(
            "git_import_metadata_required",
            format!("No Git SHA map exists at {map_path}"),
            format!("Build the SHA map with `{command}`, then retry."),
            format!("git import metadata is missing at {map_path}"),
            "reasoning import cannot map transcript references to Git commits without the SHA map",
            "repository state, refs, metadata, and worktree files were left unchanged",
            command.clone(),
            vec![command],
        )
    }

    pub(crate) fn adopt_path_conflict(positional: &str, repo_path: &str) -> Self {
        Self::invalid_usage(
            "adopt_path_conflict",
            format!(
                "`heddle adopt` received both a positional path ({positional}) and --repo ({repo_path})"
            ),
            "Pass exactly one repository path so adoption targets a single Git worktree.",
            "heddle adopt <path>",
        )
    }

    pub(crate) fn adopt_requires_git_worktree(details: Option<String>) -> Self {
        let error = match details {
            Some(details) => format!("`heddle adopt` needs a Git worktree: {details}"),
            None => "`heddle adopt` needs a Git worktree".to_string(),
        };
        Self::safety_refusal(
            "adopt_requires_git_worktree",
            error,
            "Run `heddle init` for a new native Heddle repository, or run `heddle adopt` from inside a Git worktree.",
            "the selected path is not a Git worktree",
            "adoption would otherwise initialize mapping metadata for an unknown Git checkout",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle init",
            vec!["heddle init".to_string(), "heddle status".to_string()],
        )
    }

    pub(crate) fn init_path_conflict(positional: &str, repo_path: &str) -> Self {
        Self::invalid_usage(
            "init_path_conflict",
            format!(
                "`heddle init` received both a positional path ({positional}) and --repo ({repo_path})"
            ),
            "Pass exactly one repository path so initialization targets one checkout.",
            "heddle init <path>",
        )
    }

    pub(crate) fn init_principal_field_required(field: &str) -> Self {
        Self::invalid_usage(
            "init_principal_field_required",
            format!("{field} is required when configuring a principal during init"),
            "Pass both `--principal-name` and `--principal-email`, or omit both and configure identity later.",
            "heddle init",
        )
    }

    #[cfg(not(feature = "client"))]
    pub(crate) fn network_feature_unavailable(operation: &str) -> Self {
        Self::safety_refusal(
            "network_feature_unavailable",
            format!(
                "network {operation} support is not available in this build; enable the `client` feature"
            ),
            "Use a Heddle binary built with the `client` feature for hosted network remotes, or use a local Git-overlay remote.",
            "this Heddle binary was built without hosted network transport support",
            format!("network {operation} cannot contact or mutate the requested hosted remote"),
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle remote list",
            vec![
                "heddle remote list".to_string(),
                "heddle help --output json".to_string(),
            ],
        )
    }

    pub(crate) fn hook_veto(hook: &str, action: &str, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self::safety_refusal(
            "hook_veto",
            format!("{hook} hook vetoed: {reason}"),
            format!(
                "Inspect `{hook}` with `heddle hook list`, update the hook policy or inputs, then retry."
            ),
            format!("{hook} hook vetoed {action}: {reason}"),
            format!(
                "{action} would continue after repository policy explicitly aborted the operation"
            ),
            "the operation stopped at the hook boundary before the protected action ran",
            "heddle hook list",
            vec!["heddle hook list".to_string()],
        )
    }

    pub(crate) fn invalid_usage(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self::safety_refusal(
            kind,
            error,
            hint,
            "the command arguments do not describe a valid operation",
            "running with ambiguous or invalid arguments could target the wrong repository state or metadata",
            "no repository objects, refs, metadata, or worktree files were changed",
            primary_command.clone(),
            vec![primary_command],
        )
    }

    pub(crate) fn missing_option(
        kind: &'static str,
        option: &'static str,
        required_for: &'static str,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self::invalid_usage(
            kind,
            format!("{option} is required for {required_for}"),
            format!("Retry with `{option}` set: `{primary_command}`."),
            primary_command,
        )
    }

    pub(crate) fn malformed_option_value(
        kind: &'static str,
        option: &'static str,
        raw: &str,
        expected: &'static str,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self::invalid_usage(
            kind,
            format!("{option} expects {expected}, got '{raw}'"),
            format!("Retry with {option} in the expected form: `{primary_command}`."),
            primary_command,
        )
    }

    pub(crate) fn missing_integration_target(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
        primary_command: impl Into<String>,
        recovery_commands: Vec<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self::safety_refusal(
            kind,
            error,
            hint,
            "the command has no recorded target to integrate into",
            "guessing an integration target could merge or move work into the wrong thread",
            "no repository objects, refs, metadata, or worktree files were changed",
            primary_command.clone(),
            if recovery_commands.is_empty() {
                vec![primary_command]
            } else {
                recovery_commands
            },
        )
    }

    pub(crate) fn discuss_resolve_missing_dismiss_reason() -> Self {
        Self::missing_option(
            "discuss_resolve_missing_dismiss_reason",
            "--reason",
            "dismiss",
            "heddle discuss resolve <id> --mode dismiss --reason \"...\"",
        )
    }

    pub(crate) fn review_symbols_malformed(raw: &str) -> Self {
        Self::malformed_option_value(
            "review_symbols_malformed",
            "--symbols",
            raw,
            "'file:symbol'",
            "heddle review sign <state> --kind read --symbols <file>:<symbol> --public-key <hex> --signature <hex> --signed-at-unix <secs>",
        )
    }

    pub(crate) fn thread_absorb_parent_required(thread: &str) -> Self {
        let primary_command = format!("heddle thread absorb {thread} --into <parent-thread>");
        Self::missing_integration_target(
            "thread_absorb_parent_required",
            format!("Thread '{thread}' has no recorded parent; pass --into"),
            format!(
                "Choose a parent with `heddle thread list`, then retry with `{primary_command}`."
            ),
            primary_command.clone(),
            vec![primary_command, "heddle thread list".to_string()],
        )
    }

    pub(crate) fn repository_no_head_capture_first(action: &str) -> Self {
        Self::safety_refusal(
            "repository_no_head",
            format!("Repository has no HEAD state for {action}"),
            "Capture the current worktree with `heddle capture -m \"...\"`, then retry.",
            "the repository has no current HEAD state",
            format!("`{action}` needs a concrete state id and cannot safely infer one"),
            "no repository objects, refs, metadata, or worktree files were changed",
            DIRTY_WORKTREE_CAPTURE_COMMAND,
            vec![DIRTY_WORKTREE_CAPTURE_COMMAND.to_string()],
        )
    }

    pub(crate) fn repository_no_head_anchor_first(action: &str) -> Self {
        Self::safety_refusal(
            "repository_no_head",
            format!("Repository has no HEAD state for {action}"),
            "Create a Heddle anchor with `heddle capture -m \"...\"`; commit Git-owned source history with `heddle commit -m \"...\"`, then retry.",
            "the repository has no current HEAD state",
            format!("`{action}` needs a concrete Heddle state id and cannot safely infer one"),
            "no repository objects, refs, metadata, or worktree files were changed",
            DIRTY_WORKTREE_CAPTURE_COMMAND,
            vec![
                DIRTY_WORKTREE_CAPTURE_COMMAND.to_string(),
                GIT_OVERLAY_CHECKPOINT_COMMAND.to_string(),
                "heddle status".to_string(),
            ],
        )
    }

    pub(crate) fn context_empty() -> Self {
        Self::safety_refusal(
            "context_annotations_empty",
            "No context annotations in this repository",
            "Inspect context with `heddle context list`, or add an annotation with `heddle context set --path <path> --scope file -m \"...\"`.",
            "the current state has no context annotation root",
            "guessing a missing annotation would target metadata that does not exist",
            "no repository objects, refs, metadata, or worktree files were changed",
            "heddle context list",
            vec![
                "heddle context list".to_string(),
                "heddle context set --path <path> --scope file -m \"...\"".to_string(),
            ],
        )
    }

    pub(crate) fn annotation_not_found(annotation_id: &str) -> Self {
        Self::safety_refusal(
            "context_annotation_not_found",
            format!("Annotation not found: {annotation_id}"),
            "List existing annotations with `heddle context list`, then retry with an annotation id from `heddle context get --path <path>`.",
            format!("no context annotation matched `{annotation_id}` in the current state"),
            "guessing an annotation id could inspect or mutate the wrong context metadata",
            "no repository objects, refs, metadata, or worktree files were changed",
            "heddle context list",
            vec![
                "heddle context list".to_string(),
                "heddle context get --path <path>".to_string(),
            ],
        )
    }

    pub(crate) fn no_current_thread(
        command: &'static str,
        explicit_selector: Option<&'static str>,
        primary_command: impl Into<String>,
    ) -> Self {
        let error = match explicit_selector {
            Some(selector) => format!("No current thread; pass {selector}"),
            None => "No current thread".to_string(),
        };
        let hint = match explicit_selector {
            Some(selector) => format!(
                "Run `heddle {command}` from an active thread checkout, or retry with `{selector}` to choose a thread explicitly."
            ),
            None => format!("Run `heddle {command}` from an active thread checkout."),
        };
        Self::safety_refusal(
            "no_current_thread",
            error,
            hint,
            "the current checkout is not associated with an active thread",
            format!(
                "`heddle {command}` without an explicit thread would have to guess which thread to target"
            ),
            "no repository objects, refs, metadata, or worktree files were changed",
            primary_command,
            Vec::new(),
        )
    }

    pub(crate) fn no_current_session(
        command: &'static str,
        explicit_selector: Option<&'static str>,
        primary_command: impl Into<String>,
    ) -> Self {
        let error = match explicit_selector {
            Some(selector) => format!("No active session; pass {selector}"),
            None => "No active session".to_string(),
        };
        let hint = match explicit_selector {
            Some(selector) => format!(
                "Begin provenance with `heddle agent provenance begin`, or retry `heddle {command}` with `{selector}` to choose a session explicitly."
            ),
            None => {
                "Begin provenance with `heddle agent provenance begin`, then retry.".to_string()
            }
        };
        Self::safety_refusal(
            "no_current_session",
            error,
            hint,
            "no active session is recorded for this repository",
            format!(
                "`heddle {command}` without an explicit session would have to guess which session to use"
            ),
            "no session metadata, repository objects, refs, or worktree files were changed",
            primary_command,
            Vec::new(),
        )
    }

    pub(crate) fn thread_worktree_unavailable(
        thread: &str,
        action: &str,
        unsafe_condition: impl Into<String>,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self::safety_refusal(
            "thread_worktree_unavailable",
            format!("Thread `{thread}` has no available filesystem checkout"),
            format!(
                "Use `{primary_command}` to create or inspect an on-disk checkout for this thread."
            ),
            unsafe_condition,
            format!(
                "`heddle {action}` needs a concrete directory path and cannot safely guess one"
            ),
            "repository objects, refs, metadata, and worktree files were left unchanged",
            primary_command.clone(),
            vec![primary_command, "heddle thread list".to_string()],
        )
    }

    pub(crate) fn land_checkpoint_partial_failure(
        thread: &str,
        checkpoint_error: impl fmt::Display,
        performed_steps: Vec<String>,
    ) -> Self {
        let completed = if performed_steps.is_empty() {
            "no completed steps were recorded".to_string()
        } else {
            performed_steps.join(", ")
        };
        Self::safety_refusal(
            "land_checkpoint_partial_failure",
            format!(
                "Land partially completed for `{thread}`, but Git checkpoint failed: {checkpoint_error}"
            ),
            "Run `heddle undo` to roll back the local land, or resolve the Git issue and run `heddle commit -m \"...\"`.",
            "Git checkpoint failed after Heddle had already completed local land steps",
            "retrying blindly could obscure the already-landed local merge state",
            format!("completed steps: {completed}. No Git checkpoint was written."),
            "heddle undo",
            vec![
                "heddle undo".to_string(),
                "heddle commit -m \"...\"".to_string(),
            ],
        )
    }

    pub(crate) fn from_git_projection_error(
        error: &heddle_git_projection::git_core::GitProjectionError,
    ) -> Option<Self> {
        use heddle_git_projection::git_core::GitProjectionError;
        match error {
            GitProjectionError::NonFastForwardRef { name, .. }
                if name == heddle_git_projection::git_notes::NOTES_REF =>
            {
                Some(Self::git_overlay_note_ref_conflict())
            }
            GitProjectionError::NonFastForwardRef { name, .. } => name
                .strip_prefix("refs/heads/")
                .map(Self::git_overlay_remote_push_rejected),
            GitProjectionError::MappingConflict { .. } => {
                Some(Self::git_overlay_mapping_conflict())
            }
            GitProjectionError::GitHeddleThreadDiverged { thread, branch, .. } => {
                Some(Self::git_heddle_thread_diverged(thread, branch))
            }
            GitProjectionError::RemoteDiverged {
                branch, upstream, ..
            } => Some(Self::git_overlay_remote_diverged(branch, upstream)),
            GitProjectionError::ShallowClone {
                repository,
                retry_command,
            } => Some(Self::git_overlay_shallow_clone(repository, retry_command)),
            _ => None,
        }
    }

    pub(crate) fn git_overlay_note_ref_conflict() -> Self {
        Self::safety_refusal(
            "git_overlay_note_ref_conflict",
            "Remote Heddle notes do not fast-forward",
            "Pull the remote Heddle metadata, then retry the push. If the conflict remains, create a fresh Heddle clone from the remote so Git-to-Heddle identity metadata stays authoritative.",
            "updating refs/notes/heddle would replace remote Git-to-Heddle identity metadata instead of fast-forwarding it",
            "pushing would remap commits that another Heddle checkout already identified",
            "remote refs/notes/heddle was left unchanged",
            "heddle pull",
            vec![
                "heddle pull".to_string(),
                "heddle push".to_string(),
                "heddle clone <remote> <fresh-path>".to_string(),
            ],
        )
    }

    pub(crate) fn git_overlay_mapping_conflict() -> Self {
        Self::safety_refusal(
            "git_overlay_mapping_conflict",
            "Git Projection Mapping metadata disagrees with refs/notes/heddle",
            "Git Projection Mapping and refs/notes/heddle disagree about Git-to-Heddle identity. Use a fresh Heddle clone from the remote, or restore the notes ref from the checkout whose mapping is authoritative before retrying.",
            "one Git commit maps to different Heddle change ids across Git Projection Mapping and refs/notes/heddle",
            "continuing would corrupt or hide the Git/Heddle identity mapping",
            "the command stopped before applying the requested ref or worktree update",
            "heddle clone <remote> <fresh-path>",
            vec!["heddle clone <remote> <fresh-path>".to_string()],
        )
    }

    pub(crate) fn git_overlay_shallow_clone(
        repository: &std::path::Path,
        retry_command: &str,
    ) -> Self {
        let primary_command = "heddle clone <remote> <fresh-path>".to_string();
        Self::safety_refusal(
            "git_overlay_shallow_clone",
            "Shallow Git repository cannot be imported",
            format!(
                "Import needs complete ancestry. Create or choose a complete checkout with `{primary_command}`, then retry with `{retry_command}`."
            ),
            format!(
                "Git repository '{}' is shallow and does not contain the full commit ancestry Heddle needs to preserve stable change identity",
                repository.display()
            ),
            "importing from this checkout would create an incomplete Git-to-Heddle history map",
            "Heddle refs, Git refs, index, and worktree files were left unchanged",
            primary_command.clone(),
            vec![primary_command, retry_command.to_string()],
        )
    }

    pub(crate) fn git_heddle_thread_diverged(thread: &str, branch: &str) -> Self {
        let primary_command = canonical_git_repair_ref_preview_command(None, branch);
        Self::safety_refusal(
            "git_heddle_thread_diverged",
            "Git branch and Heddle thread have diverged",
            format!(
                "Inspect the local repair choices with `{primary_command}`. Preview mode does not move refs, update the index, change worktree files, push, or pull."
            ),
            format!(
                "Heddle thread '{thread}' and Git branch '{branch}' both contain history the other side lacks"
            ),
            "importing or syncing now would need to choose whether the local Git branch or Heddle thread is authoritative",
            "Heddle thread refs, Git refs, index, and worktree files were left unchanged; imported commit states and Git/Heddle mapping records may have been preserved for inspection or retry",
            primary_command.clone(),
            vec![primary_command],
        )
    }

    pub(crate) fn git_overlay_remote_push_rejected(branch: &str) -> Self {
        let primary_command = "heddle pull".to_string();
        Self::safety_refusal(
            "git_overlay_remote_diverged",
            "Remote branch does not fast-forward the local Git checkpoint",
            "Run `heddle pull` so Heddle can stream and inspect the remote tip, then run `heddle verify` for the exact integration command.",
            format!(
                "pushing branch '{branch}' would rewrite the remote branch instead of fast-forwarding it"
            ),
            "pushing now would replace work that exists on the remote",
            "the remote branch, local Git branch, Heddle refs, index, and worktree files were left unchanged",
            primary_command.clone(),
            vec![primary_command, "heddle verify".to_string()],
        )
    }

    pub(crate) fn git_overlay_remote_diverged(branch: &str, upstream: &str) -> Self {
        let import_command = canonical_git_import_ref_command(upstream);
        let merge_preview = super::thread_landing::merge_preview_command(upstream);
        Self::safety_refusal(
            "git_overlay_remote_diverged",
            "Remote branch does not fast-forward the local Git checkpoint",
            format!(
                "Import the fetched upstream tip with `{import_command}`, then preview integration with `{merge_preview}`."
            ),
            format!(
                "local branch '{branch}' and upstream '{upstream}' both contain commits the other side lacks"
            ),
            "pulling now would need to integrate upstream work with local Heddle work before moving the branch",
            "Heddle refs, the visible Git branch, and worktree files were left unchanged",
            import_command.clone(),
            vec![import_command, merge_preview],
        )
    }

    pub(crate) fn remote_transport_mismatch(action: &str, remote: &str) -> Self {
        Self::safety_refusal(
            "remote_transport_mismatch",
            format!(
                "Refusing to {action}: remote '{remote}' is a Git remote, not a Heddle-native remote"
            ),
            "Use a Heddle-native remote here, or clone/adopt that Git remote in a Git-overlay checkout.",
            format!("remote '{remote}' resolves to Git storage"),
            format!(
                "{action} would route a Git repository through Heddle-native sync and fail after setup work"
            ),
            "remote configuration, Heddle refs, Git refs, and worktree files were left unchanged",
            "heddle clone <remote> <fresh-path>",
            vec![
                "heddle clone <remote> <fresh-path>".to_string(),
                "heddle remote add <name> <url>".to_string(),
            ],
        )
    }

    pub(crate) fn remote_not_configured(action: &str) -> Self {
        Self::safety_refusal(
            "remote_not_configured",
            format!("No default remote is configured for {action}"),
            format!(
                "Add a remote with `heddle remote add <name> <url>`, inspect remotes with `heddle remote list`, or choose one with `heddle remote set-default <name>`. Ad-hoc targets are supported without configuration: `heddle {action} <remote>` accepts a remote name, URL, local path, or hosted address positionally."
            ),
            "the command did not receive a remote argument and no default remote is configured",
            format!(
                "{action} needs a concrete remote target before it can move remote refs or transfer objects"
            ),
            "repository state, refs, remote configuration, and worktree files were left unchanged",
            "heddle remote add <name> <url>",
            vec![
                "heddle remote add <name> <url>".to_string(),
                "heddle remote list".to_string(),
                "heddle remote set-default <name>".to_string(),
            ],
        )
    }

    pub(crate) fn remote_not_found(name: &str) -> Self {
        Self::safety_refusal(
            "remote_not_found",
            format!("Remote '{name}' not found"),
            "Inspect configured remotes with `heddle remote list`, or add one with `heddle remote add <name> <url>`.",
            format!("no configured Heddle or Git remote named '{name}' was found"),
            "the command cannot inspect, fetch, pull, or push a remote until the remote name is resolved",
            "remote configuration, refs, objects, and worktree files were left unchanged",
            "heddle remote list",
            vec![
                "heddle remote list".to_string(),
                "heddle remote add <name> <url>".to_string(),
            ],
        )
    }

    pub(crate) fn local_lazy_pull_unsupported(source_path: &std::path::Path) -> Self {
        let source = source_path.display().to_string();
        let pull_without_lazy = format!("heddle pull {source}");
        Self::safety_refusal(
            "local_lazy_pull_unsupported",
            "Refusing lazy pull from local remote: lazy materialization requires a hosted or network remote",
            format!(
                "Run `{pull_without_lazy}` without `--lazy`, or configure a hosted remote and retry lazy pull there."
            ),
            format!("selected remote resolves to local path file://{source}"),
            "lazy pull would leave the worktree depending on on-demand object fetches that the local transport does not provide",
            "repository state was left unchanged",
            pull_without_lazy.clone(),
            vec![pull_without_lazy, "heddle remote list".to_string()],
        )
    }

    #[cfg(feature = "client")]
    pub(crate) fn remote_push_failed(track_name: &str, error: &str) -> Self {
        let primary_command = format!("heddle push {track_name}");
        Self::safety_refusal(
            "remote_push_failed",
            format!("Push failed for {track_name}: {error}"),
            format!(
                "Inspect `heddle verify`, then retry with `{primary_command}` after fixing the remote."
            ),
            format!("remote push to {track_name} failed: {error}"),
            "the remote branch was not confirmed updated",
            "local Heddle state, Git refs, and worktree files were left unchanged by the failed push result",
            primary_command.clone(),
            vec![primary_command, "heddle verify".to_string()],
        )
    }

    #[cfg(feature = "client")]
    pub(crate) fn remote_pull_failed(
        remote_thread: &str,
        local_thread: Option<&str>,
        error: &str,
    ) -> Self {
        let primary_command = if let Some(local_thread) = local_thread {
            format!("heddle pull {remote_thread} {local_thread}")
        } else {
            format!("heddle pull {remote_thread}")
        };
        Self::safety_refusal(
            "remote_pull_failed",
            format!("Pull failed from {remote_thread}: {error}"),
            format!(
                "Inspect `heddle verify`, then retry with `{primary_command}` after fixing the remote."
            ),
            format!("remote pull from {remote_thread} failed: {error}"),
            "the local thread was not confirmed updated from the remote",
            "local Heddle state, Git refs, and worktree files were left unchanged by the failed pull result",
            primary_command.clone(),
            vec![primary_command, "heddle verify".to_string()],
        )
    }

    #[cfg(feature = "client")]
    pub(crate) fn network_clone_failed(error: &str, local_path: &std::path::Path) -> Self {
        Self::safety_refusal(
            "network_clone_failed",
            format!("Clone failed: {error}"),
            "Check the remote, credentials, and requested ref, then retry `heddle clone`.",
            format!(
                "network clone reported failure for '{}': {error}",
                local_path.display()
            ),
            "clone cannot prove that all requested remote objects and refs were materialized",
            "any created destination files or metadata were left for inspection",
            "heddle clone <remote> <path>",
            vec!["heddle clone <remote> <path>".to_string()],
        )
    }

    /// `thread refresh` was asked to refresh a thread that has no
    /// recorded `target_thread`. The thread metadata lives on disk but
    /// the integration target was never written, so the refresh has no
    /// concrete destination to rebase onto.
    ///
    /// Surfaces Priya's #1 untyped error site: the bare
    /// `Thread '<id>' has no target thread` line gave the operator no
    /// next step. The typed envelope hands back the inspection commands
    /// (`heddle thread show`, `heddle thread list`) and the
    /// re-configuration command shape so the JSON envelope's
    /// `recovery_action_templates` field carries executable argv.
    pub(crate) fn missing_target_thread(thread_id: &str, attempted_verb: &str) -> Self {
        let show_command = format!("heddle thread show {thread_id}");
        let refresh_command = format!("heddle thread refresh {thread_id}");
        Self::safety_refusal(
            "missing_target_thread",
            format!("Thread '{thread_id}' has no target thread"),
            format!(
                "Inspect the thread's metadata with `{show_command}` to see which target_thread to set, then retry the refresh once the thread has a recorded target."
            ),
            format!(
                "{attempted_verb} was requested for thread '{thread_id}', but the thread record has no `target_thread` field"
            ),
            format!(
                "{attempted_verb} needs a concrete target thread to rebase onto and cannot safely guess one"
            ),
            "no thread refs, checkout directories, mounts, or agent reservations were changed",
            show_command.clone(),
            vec![
                show_command,
                refresh_command,
                "heddle thread list".to_string(),
            ],
        )
    }

    /// A rebase replay step referenced a state, commit, or tree the
    /// object store no longer has. Usually means a pruning operation
    /// ran between rebase start and rebase resume, or the persisted
    /// `REBASE_STATE` references objects from a sibling repository.
    /// Abort recovers — the tolerant loader will rewind to
    /// `original_head` without needing the missing objects.
    pub(crate) fn rebase_referenced_state_missing(state_id: &str, role: &str) -> Self {
        let primary = "heddle abort".to_string();
        Self::safety_refusal(
            "rebase_referenced_state_missing",
            format!("{role} '{state_id}' not found while continuing rebase"),
            format!(
                "Abort with `{primary}` to rewind to the pre-rebase head, then inspect the object store with `heddle log` and `heddle maintenance gc --dry-run` before restarting the rebase."
            ),
            format!(
                "rebase replay referenced {role} '{state_id}', but the object store does not contain it (possibly pruned or imported from a sibling repository)"
            ),
            "continuing the rebase without the referenced object could replay against the wrong tree or leave the rebase half-applied",
            "the working tree, refs, and rebase state were left at the failure point so the abort path can rewind cleanly",
            primary.clone(),
            vec![primary, "heddle log".to_string()],
        )
    }

    /// A persisted `REBASE_STATE` file could not be parsed or violated
    /// an invariant. The strict loader (used by `--continue`) hard-fails
    /// so a half-applied batch never reaches the oplog; the tolerant
    /// loader (used by `--abort`) absorbs most of these cases and rewinds
    /// to `original_head`.
    ///
    /// `field` describes which part of REBASE_STATE failed validation
    /// (e.g. `"Missing 'onto'"`, `"decode pending_advance"`,
    /// `"Inconsistent rebase state"`); `detail` carries the underlying
    /// cause when there is one (e.g. a hex/msgpack decode error) and may
    /// be empty. Tests assert on the `field` substring, so the user-
    /// visible `error` always starts with `field`.
    pub(crate) fn rebase_state_corrupted(field: &str, detail: impl fmt::Display) -> Self {
        let primary = "heddle abort".to_string();
        let detail_str = detail.to_string();
        let error = if detail_str.trim().is_empty() {
            field.to_string()
        } else {
            format!("{field}: {detail_str}")
        };
        Self::safety_refusal(
            "rebase_state_corrupted",
            error.clone(),
            format!(
                "Abort with `{primary}` — the tolerant loader rewinds to the pre-rebase head even when the strict --continue loader rejects this file."
            ),
            format!("REBASE_STATE failed strict validation: {error}"),
            "resuming a corrupted rebase could replay an incomplete batch or commit a blank transaction id, polluting the oplog",
            "the working tree, refs, and rebase state were left untouched so the abort path can read original_head",
            primary.clone(),
            vec![primary],
        )
    }

    /// A thread command resolved a state spec or anchor and the
    /// referenced state was not in the object store. Distinct from
    /// `state_not_found` because the lookup happens inside thread
    /// command flow (start/create/anchor) rather than the generic state
    /// resolver.
    pub(crate) fn thread_referenced_state_missing(state_id: &str, role: &str) -> Self {
        let show = format!("heddle thread show {state_id}");
        Self::safety_refusal(
            "thread_referenced_state_missing",
            format!("{role} '{state_id}' not found"),
            format!(
                "List recent states with `heddle log` to find a reachable id, inspect threads with `heddle thread list`, or use `{show}` if the id is a thread name."
            ),
            format!("{role} '{state_id}' is not in the object store"),
            "starting or anchoring a thread to a missing state would create thread metadata that no inspection path can resolve",
            "thread refs, checkout directories, and thread metadata were left unchanged",
            "heddle log",
            vec![
                "heddle log".to_string(),
                "heddle thread list".to_string(),
                show,
            ],
        )
    }

    /// `--print-cd-path` (or another path-only output mode) was passed
    /// to a thread command but the thread has no on-disk worktree to
    /// `cd` into. Lightweight (non-materialized) threads do not own a
    /// directory — the operator needs to materialize the thread or use
    /// a different command that doesn't require a checkout path.
    pub(crate) fn thread_checkout_unavailable(thread_name: &str, attempted_verb: &str) -> Self {
        let start_command = format!("heddle thread start {thread_name} --path ../{thread_name}");
        let show_command = format!("heddle thread show {thread_name}");
        Self::safety_refusal(
            "thread_checkout_unavailable",
            format!(
                "thread '{thread_name}' has no on-disk worktree; `--print-cd-path` only works for materialized threads"
            ),
            format!(
                "Materialize the thread with `{start_command}`, or inspect its mode with `{show_command}` to see whether it should be promoted from lightweight."
            ),
            format!(
                "{attempted_verb} requires a concrete filesystem path, but thread '{thread_name}' is registered as lightweight (no materialized checkout)"
            ),
            format!(
                "{attempted_verb} cannot print a checkout path for a thread that never had one materialized"
            ),
            "thread refs, checkout directories, and thread metadata were left unchanged",
            show_command.clone(),
            vec![show_command, "heddle thread list".to_string()],
        )
    }

    pub fn primary_hint(&self) -> &str {
        &self.hint
    }
}

pub(crate) fn dirty_worktree_recovery_commands() -> Vec<String> {
    vec![DIRTY_WORKTREE_CAPTURE_COMMAND.to_string()]
}

fn hex_hash(hash: [u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl fmt::Display for RecoveryAdvice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}. Unsafe: {}. Would change: {}. Preserved: {}. Primary recovery: `{}`.",
            self.error,
            self.unsafe_condition,
            self.would_change,
            self.preserved,
            self.primary_command
        )?;
        if self.recovery_commands.len() > 1 {
            write!(f, " Other recovery: ")?;
            for (index, command) in self.recovery_commands.iter().enumerate().skip(1) {
                if index > 1 {
                    write!(f, ", ")?;
                }
                write!(f, "`{command}`")?;
            }
            write!(f, ".")?;
        }
        Ok(())
    }
}

impl Error for RecoveryAdvice {}

#[cfg(test)]
mod tests {
    use super::RecoveryAdvice;
    use heddle_git_projection::git_core::GitProjectionError;

    #[test]
    fn git_projection_mapping_conflict_returns_typed_advice() {
        let error = GitProjectionError::MappingConflict {
            message: "git oid abc mapped to old-change (new new-change)".to_string(),
        };

        let advice = RecoveryAdvice::from_git_projection_error(&error)
            .expect("mapping conflict should be classified");

        assert_eq!(advice.kind, "git_overlay_mapping_conflict");
        assert_eq!(advice.primary_command, "heddle clone <remote> <fresh-path>");
        assert!(
            advice
                .unsafe_condition
                .contains("one Git commit maps to different Heddle change ids")
        );
    }

    #[test]
    fn git_projection_plain_conflict_message_does_not_classify_as_mapping_conflict() {
        let error = GitProjectionError::Conflict(
            "git oid abc mapped to old-change (new new-change)".to_string(),
        );

        assert!(RecoveryAdvice::from_git_projection_error(&error).is_none());
    }

    #[test]
    fn git_projection_shallow_clone_returns_typed_advice() {
        let retry_command = "heddle import git --ref main";
        let error = GitProjectionError::ShallowClone {
            repository: std::path::PathBuf::from("/tmp/shallow"),
            retry_command: retry_command.to_string(),
        };

        let advice = RecoveryAdvice::from_git_projection_error(&error)
            .expect("shallow clone should be classified");

        assert_eq!(advice.kind, "git_overlay_shallow_clone");
        assert_eq!(
            advice.recovery_commands,
            vec![
                "heddle clone <remote> <fresh-path>".to_string(),
                retry_command.to_string()
            ]
        );
        assert!(
            !advice.hint.contains("git fetch"),
            "shallow recovery should stay no-git friendly: {}",
            advice.hint
        );
    }

    #[test]
    fn rejected_git_push_recovers_through_heddle_pull() {
        let advice = RecoveryAdvice::git_overlay_remote_push_rejected("main");

        assert_eq!(advice.primary_command, "heddle pull");
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle pull", "heddle verify"]
        );
        assert!(!advice.hint.contains("git fetch"));
    }
}
