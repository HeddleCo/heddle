// SPDX-License-Identifier: Apache-2.0
//! Typed refusal and recovery advice shared by command surfaces.

use std::{error::Error, fmt};

use serde_json::{Map, Value};

pub(crate) const DIRTY_WORKTREE_COMMIT_COMMAND: &str = "heddle commit -m \"...\"";
pub(crate) const DIRTY_WORKTREE_CAPTURE_COMMAND: &str = "heddle capture -m \"...\"";
pub(crate) const DIRTY_WORKTREE_STASH_COMMAND: &str = "heddle stash push -m \"...\"";

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
        let primary_command = DIRTY_WORKTREE_COMMIT_COMMAND.to_string();
        Self {
            kind: "dirty_worktree",
            error: format!("Save or stash worktree changes before {action}"),
            hint: format!(
                "Save the work with `{primary_command}`; use `{DIRTY_WORKTREE_CAPTURE_COMMAND}` for a Heddle-only recovery point or park it with `{DIRTY_WORKTREE_STASH_COMMAND}`, then retry."
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
        let primary_command =
            format!("heddle bridge git reconcile --prefer heddle --ref {branch} --preview");
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
            primary_command: "heddle commands --output json".to_string(),
            recovery_commands: vec!["heddle commands --output json".to_string()],
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
            hint: "Inspect op-id support with `heddle commands --output json` and retry without --op-id for this command.".to_string(),
            unsafe_condition: "the command contract marks this command as not idempotent".to_string(),
            would_change: "silently accepting --op-id here would imply a replay guarantee this command does not provide".to_string(),
            preserved: "no command body was executed".to_string(),
            primary_command: "heddle commands --output json".to_string(),
            recovery_commands: vec!["heddle commands --output json".to_string()],
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
            primary_command: "heddle commands --output json".to_string(),
            recovery_commands: vec!["heddle commands --output json".to_string()],
            extra_json_fields: Map::new(),
        }
    }

    pub fn json_unsupported(command: &str) -> Self {
        Self {
            kind: "json_unsupported",
            error: format!("--output json is not supported by `heddle {command}`"),
            hint: "Inspect JSON-capable commands with `heddle commands --output json` or rerun with `--output text`.".to_string(),
            unsafe_condition: "the command contract marks this command as text-only".to_string(),
            would_change: "emitting ad hoc JSON here would create a machine contract outside the command table".to_string(),
            preserved: "no command body was executed".to_string(),
            primary_command: "heddle commands --output json".to_string(),
            recovery_commands: vec![
                "heddle commands --output json".to_string(),
                format!("heddle {command} --output text"),
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

    pub(crate) fn merge_integrity_refusal(
        error: impl Into<String>,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        preserved: impl Into<String>,
    ) -> Self {
        Self::safety_refusal(
            "repository_integrity_error",
            error,
            "Inspect repository integrity with `heddle fsck --full`, then restore or repair the reported object/ref.",
            unsafe_condition,
            would_change,
            preserved,
            "heddle fsck --full",
            vec!["heddle fsck --full".to_string()],
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

    pub(crate) fn bridge_ingest_required(map_path: &str, git_path: &str) -> Self {
        let command = format!("heddle bridge ingest --path {git_path}");
        Self::safety_refusal(
            "bridge_ingest_required",
            format!("No Git SHA map exists at {map_path}"),
            format!("Build the SHA map with `{command}`, then retry."),
            format!("bridge ingest metadata is missing at {map_path}"),
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
                "heddle commands --output json".to_string(),
            ],
        )
    }

    pub(crate) fn git_remote_name_invalid(name: &str) -> Self {
        Self::invalid_usage(
            "git_remote_name_invalid",
            format!("invalid Git remote name for Git-overlay repository: {name}"),
            "Use a Git remote name without spaces, control characters, path separators, ref metacharacters, leading dots, or a `.lock` suffix.",
            "heddle remote list",
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

    #[cfg(not(feature = "semantic"))]
    pub(crate) fn feature_unavailable(command: &str, feature: &str) -> Self {
        Self::safety_refusal(
            "feature_unavailable",
            format!("{command} requires building heddle with --features {feature}"),
            format!(
                "Use a heddle binary built with `--features {feature}`, or rerun without the feature-specific flag."
            ),
            format!("this heddle binary was built without the `{feature}` feature"),
            format!("{command} cannot run because the requested analysis engine is unavailable"),
            "repository state, refs, and worktree files were left unchanged",
            "heddle commands --output json",
            vec!["heddle commands --output json".to_string()],
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

    pub(crate) fn discuss_resolve_missing_annotation_kind() -> Self {
        Self::missing_option(
            "discuss_resolve_missing_annotation_kind",
            "--annotation-kind",
            "into-annotation",
            "heddle discuss resolve <id> --mode into-annotation --annotation-kind rationale --annotation-content \"...\"",
        )
    }

    pub(crate) fn discuss_resolve_missing_annotation_content() -> Self {
        Self::missing_option(
            "discuss_resolve_missing_annotation_content",
            "--annotation-content",
            "into-annotation",
            "heddle discuss resolve <id> --mode into-annotation --annotation-kind rationale --annotation-content \"...\"",
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
                "Start a session with `heddle session start`, or retry `heddle {command}` with `{selector}` to choose a session explicitly."
            ),
            None => "Start a session with `heddle session start`, then retry.".to_string(),
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

    pub(crate) fn no_attached_parent_thread() -> Self {
        Self::safety_refusal(
            "no_attached_parent_thread",
            "No attached parent thread; pass --parent",
            "Run `heddle delegate --parent <THREAD> <task>` from a detached checkout, or switch into an attached thread first.",
            "the current checkout is detached and no parent thread was supplied",
            "`heddle delegate` without a parent would have to guess which thread should own the delegated work",
            "no delegated threads, refs, metadata, or worktree files were changed",
            "heddle delegate --parent <THREAD> <task>",
            vec![
                "heddle delegate --parent <THREAD> <task>".to_string(),
                "heddle thread list".to_string(),
            ],
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

    pub(crate) fn ship_push_remote_missing(thread: &str) -> Self {
        Self::safety_refusal(
            "ship_push_remote_missing",
            format!("Refusing to ship thread `{thread}` with --push: no push remote is configured"),
            format!(
                "Run `heddle ship --thread {thread} --no-push` to land locally, or configure a remote and retry with `--push`."
            ),
            "no default Git or Heddle remote is configured for push",
            "shipping with --push would merge and checkpoint before discovering there is nowhere to push",
            "repository state, refs, metadata, and worktree files were left unchanged",
            format!("heddle ship --thread {thread} --no-push"),
            vec![
                format!("heddle ship --thread {thread} --no-push"),
                "heddle remote add <name> <url>".to_string(),
            ],
        )
    }

    pub(crate) fn ship_remote_requires_push(thread: &str, remote: &str) -> Self {
        let push_command = format!("heddle ship --thread {thread} --push --remote {remote}");
        let local_command = format!("heddle ship --thread {thread} --no-push");
        Self::safety_refusal(
            "ship_remote_requires_push",
            format!("Ship remote `{remote}` was provided without --push"),
            format!(
                "Run `{push_command}` to land and publish, or `{local_command}` to land locally."
            ),
            "`--remote` names a push destination, but this ship invocation did not request a push",
            "continuing would merge/checkpoint locally while leaving the named remote unchanged",
            "repository state, refs, metadata, remote refs, and worktree files were left unchanged",
            push_command.clone(),
            vec![push_command, local_command],
        )
    }

    pub(crate) fn ship_push_option_conflict(thread: &str) -> Self {
        let push_command = format!("heddle ship --thread {thread} --push");
        let local_command = format!("heddle ship --thread {thread} --no-push");
        Self::safety_refusal(
            "ship_push_option_conflict",
            "Ship was asked to both push and not push",
            format!("Choose `{push_command}` or `{local_command}`."),
            "`--push` and `--no-push` are mutually exclusive publish choices",
            "continuing would make the publish side effect ambiguous",
            "repository state, refs, metadata, remote refs, and worktree files were left unchanged",
            local_command.clone(),
            vec![push_command, local_command],
        )
    }

    pub(crate) fn ship_push_partial_failure(
        thread: &str,
        push_error: impl fmt::Display,
        performed_steps: Vec<String>,
        git_commit: Option<&str>,
        attempted_remote: Option<&str>,
    ) -> Self {
        let completed = if performed_steps.is_empty() {
            "no completed steps were recorded".to_string()
        } else {
            performed_steps.join(", ")
        };
        let checkpoint = git_commit
            .map(|oid| format!(" Git checkpoint {oid} was written."))
            .unwrap_or_default();
        let push_command = attempted_remote
            .filter(|remote| !remote.trim().is_empty())
            .map(|remote| format!("heddle push {remote}"))
            .unwrap_or_else(|| "heddle push".to_string());
        Self::safety_refusal(
            "ship_push_partial_failure",
            format!("Ship partially completed for `{thread}`, but push failed: {push_error}"),
            format!(
                "The thread was preserved locally. Run `heddle undo` to roll back the local ship, or fix the remote and run `{push_command}`."
            ),
            "push failed after Heddle had already completed local ship steps",
            "retrying blindly could duplicate or obscure the already-landed local merge/checkpoint",
            format!("completed steps: {completed}.{checkpoint}"),
            "heddle undo",
            vec!["heddle undo".to_string(), push_command],
        )
    }

    pub(crate) fn ship_checkpoint_partial_failure(
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
            "ship_checkpoint_partial_failure",
            format!(
                "Ship partially completed for `{thread}`, but Git checkpoint failed: {checkpoint_error}"
            ),
            "Run `heddle undo` to roll back the local ship, or resolve the checkpoint issue and run `heddle checkpoint -m \"...\"`.",
            "Git checkpoint failed after Heddle had already completed local ship steps",
            "retrying blindly could obscure the already-landed local merge state",
            format!("completed steps: {completed}. No Git checkpoint was written."),
            "heddle undo",
            vec![
                "heddle undo".to_string(),
                "heddle checkpoint -m \"...\"".to_string(),
            ],
        )
    }

    pub fn primary_hint(&self) -> &str {
        &self.hint
    }
}

pub(crate) fn dirty_worktree_recovery_commands() -> Vec<String> {
    vec![
        DIRTY_WORKTREE_COMMIT_COMMAND.to_string(),
        DIRTY_WORKTREE_CAPTURE_COMMAND.to_string(),
        DIRTY_WORKTREE_STASH_COMMAND.to_string(),
    ]
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
