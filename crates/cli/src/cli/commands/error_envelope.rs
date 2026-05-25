// SPDX-License-Identifier: Apache-2.0
//! Shared stderr error envelopes for CLI failures.

use clap::error::Error as ClapError;

use super::{
    RecoveryAdvice,
    command_catalog::{ActionTemplate, recommended_action_template},
    recommended_action_argv,
};
use crate::cli::{Cli, render::shell_quote, should_output_json};

/// Print an error to stderr with a one-line next-step hint when the error
/// chain matches a known recoverable condition. Stays out of the way
/// otherwise — `anyhow`'s `Debug` impl is good enough for arbitrary errors.
///
/// Honours the resolved output format: when JSON is selected, emits a
/// single-line structured envelope instead of freeform text so scripts can
/// parse it cleanly. The envelope is a stderr-only contract — the stdout schemas in
/// `crates/cli/src/cli/commands/schemas.rs` are untouched.
pub fn print_error_with_hint(cli: &Cli, err: &anyhow::Error) {
    let classification = classify_error(err);
    let hint = classification.hint.clone();
    let kind = classification.kind.clone();
    let error = display_error_message(err, &kind);
    let json = should_output_json(cli, None);
    if json {
        let envelope_error = classification
            .human_error
            .as_deref()
            .unwrap_or(error.as_str());
        let primary_command_argv = command_argv(&classification.primary_command);
        let primary_command_template = command_template(&classification.primary_command);
        let recovery_command_argv = command_argvs(&classification.recovery_commands);
        let recovery_action_templates = command_templates(&classification.recovery_commands);
        let mut body = serde_json::json!({
            "code": kind,
            "error": envelope_error,
            "exit_code": 1,
            "hint": hint,
            "kind": kind,
            "unsafe_condition": classification.unsafe_condition,
            "would_change": classification.would_change,
            "preserved": classification.preserved,
            "primary_command": classification.primary_command,
            "primary_command_argv": primary_command_argv,
            "primary_command_template": primary_command_template,
            "recovery_commands": classification.recovery_commands,
            "recovery_command_argv": recovery_command_argv,
            "recovery_action_templates": recovery_action_templates,
        });
        if let Some(op_id) = cli.op_id.as_deref()
            && let Some(object) = body.as_object_mut()
        {
            object.insert(
                "op_id".to_string(),
                serde_json::Value::String(op_id.to_string()),
            );
            object.insert(
                "idempotency_status".to_string(),
                serde_json::Value::String(idempotency_status_for_error(&kind).to_string()),
            );
            object.insert("replayed".to_string(), serde_json::Value::Bool(false));
        }
        if let Some(object) = body.as_object_mut() {
            for (key, value) in classification.extra_json_fields {
                object.insert(key, value);
            }
        }
        eprintln!("{body}");
    } else {
        eprintln!(
            "Error: {}",
            classification
                .human_error
                .as_deref()
                .unwrap_or(error.as_str())
        );
        eprintln!("Next: {}", classification.primary_command);
        if matches!(
            kind.as_str(),
            "dirty_worktree" | "source_thread_uncaptured_work"
        ) && cli.verbose == 0
        {
            eprintln!(
                "Paths: {}",
                compact_dirty_worktree_condition(&classification.unsafe_condition)
            );
            eprintln!("Reason: {}", classification.would_change);
            eprintln!("Kept: {}", classification.preserved);
            if classification.recovery_commands.len() > 1 {
                eprintln!("Also: {}", classification.recovery_commands[1..].join(", "));
            }
        } else if cli.verbose > 0 {
            eprintln!("Unsafe: {}", classification.unsafe_condition);
            eprintln!("Would change: {}", classification.would_change);
            eprintln!("Preserved: {}", classification.preserved);
            if classification.recovery_commands.len() > 1 {
                eprintln!(
                    "Other recovery: {}",
                    classification.recovery_commands[1..].join(", ")
                );
            }
            eprintln!("Hint: {hint}");
        }
    }
}

fn compact_dirty_worktree_condition(condition: &str) -> String {
    const PREFIX: &str = "unsaved worktree path(s): ";
    let Some(paths) = condition.strip_prefix(PREFIX) else {
        return condition.to_string();
    };
    let paths = paths
        .split(", ")
        .map(|path| {
            path.strip_prefix("modified: ")
                .or_else(|| path.strip_prefix("deleted: "))
                .or_else(|| path.strip_prefix("untracked: "))
                .unwrap_or(path)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{PREFIX}{paths}")
}

pub fn print_parse_error_json_envelope(err: &ClapError) {
    let primary_command = "heddle commands --output json";
    let recovery_commands = vec![
        primary_command.to_string(),
        "heddle help --output text".to_string(),
    ];
    let recovery_command_argv = command_argvs(&recovery_commands);
    let recovery_action_templates = command_templates(&recovery_commands);
    let body = serde_json::json!({
        "code": "parse_error",
        "error": err.to_string(),
        "exit_code": err.exit_code(),
        "hint": "Run `heddle commands --output json` to inspect the command surface.",
        "kind": "parse_error",
        "unsafe_condition": "the requested arguments do not match the registered command surface",
        "would_change": "the command body was not executed, so no repository state could be changed",
        "preserved": "no command body was executed",
        "primary_command": primary_command,
        "primary_command_argv": command_argv(primary_command),
        "primary_command_template": command_template(primary_command),
        "recovery_commands": recovery_commands,
        "recovery_command_argv": recovery_command_argv,
        "recovery_action_templates": recovery_action_templates,
    });
    eprintln!("{body}");
}

fn idempotency_status_for_error(kind: &str) -> &'static str {
    match kind {
        "op_id_conflict" => "conflict",
        "op_id_in_flight" => "in_flight",
        "op_id_invalid" => "invalid",
        "op_id_unsupported" => "unsupported",
        _ => "failed",
    }
}

fn display_error_message(err: &anyhow::Error, kind: &str) -> String {
    match kind {
        "operation_not_in_progress" => "No merge in progress".to_string(),
        _ => format!("{err:#}"),
    }
}

#[derive(Debug)]
struct ErrorClassification {
    kind: String,
    human_error: Option<String>,
    hint: String,
    unsafe_condition: String,
    would_change: String,
    preserved: String,
    primary_command: String,
    recovery_commands: Vec<String>,
    extra_json_fields: serde_json::Map<String, serde_json::Value>,
}

impl ErrorClassification {
    fn from_advice(advice: &RecoveryAdvice) -> Self {
        Self {
            kind: advice.kind.to_string(),
            human_error: Some(advice.error.clone()),
            hint: advice.primary_hint().to_string(),
            unsafe_condition: advice.unsafe_condition.clone(),
            would_change: advice.would_change.clone(),
            preserved: advice.preserved.clone(),
            primary_command: advice.primary_command.clone(),
            recovery_commands: advice.recovery_commands.clone(),
            extra_json_fields: advice.extra_json_fields.clone(),
        }
    }

    fn known(
        kind: &'static str,
        hint: impl Into<String>,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        preserved: impl Into<String>,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self {
            kind: kind.to_string(),
            human_error: None,
            hint: hint.into(),
            unsafe_condition: unsafe_condition.into(),
            would_change: would_change.into(),
            preserved: preserved.into(),
            primary_command: primary_command.clone(),
            recovery_commands: vec![primary_command],
            extra_json_fields: serde_json::Map::new(),
        }
    }

    fn known_with_error(
        kind: &'static str,
        human_error: impl Into<String>,
        hint: impl Into<String>,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        preserved: impl Into<String>,
        primary_command: impl Into<String>,
    ) -> Self {
        let mut classification = Self::known(
            kind,
            hint,
            unsafe_condition,
            would_change,
            preserved,
            primary_command,
        );
        classification.human_error = Some(human_error.into());
        classification
    }

    fn runtime() -> Self {
        Self::known(
            "runtime_error",
            "Run `heddle status` or retry with `-v` for more context.",
            "the command failed before Heddle could classify a safer recovery path",
            "retrying may repeat the same failure until the underlying cause is fixed",
            "no typed recovery advice was available; inspect the error before retrying",
            "heddle status",
        )
    }
}

fn command_argv(command: &str) -> Option<Vec<String>> {
    recommended_action_argv(command).ok().flatten()
}

fn command_template(command: &str) -> Option<ActionTemplate> {
    recommended_action_template(command)
}

fn command_argvs(commands: &[String]) -> Vec<Vec<String>> {
    commands
        .iter()
        .filter_map(|command| command_argv(command))
        .collect()
}

fn command_templates(commands: &[String]) -> Vec<ActionTemplate> {
    commands
        .iter()
        .filter_map(|command| command_template(command))
        .collect()
}

/// Match the error chain against the `HeddleError` variants and named
/// `objects::fs_atomic` predicates we promise actionable hints for. Returns
/// structured recovery details for the matched class. Generic failures still
/// get a non-empty envelope so JSON callers never have to scrape stderr.
fn classify_error(err: &anyhow::Error) -> ErrorClassification {
    use objects::error::HeddleError;
    for cause in err.chain() {
        if let Some(advice) = cause.downcast_ref::<RecoveryAdvice>() {
            return ErrorClassification::from_advice(advice);
        }
        if let Some(advice) = cause.downcast_ref::<weft_client_shim::HostedRecoveryAdvice>() {
            return ErrorClassification {
                kind: advice.kind.to_string(),
                human_error: Some(advice.error.clone()),
                hint: advice.hint.clone(),
                unsafe_condition: advice.unsafe_condition.clone(),
                would_change: advice.would_change.clone(),
                preserved: advice.preserved.clone(),
                primary_command: advice.primary_command.clone(),
                recovery_commands: advice.recovery_commands.clone(),
                extra_json_fields: serde_json::Map::new(),
            };
        }
        if let Some(git_error) = cause.downcast_ref::<crate::bridge::git_core::GitBridgeError>() {
            match git_error {
                crate::bridge::git_core::GitBridgeError::NonFastForwardRef { name, .. }
                    if name == crate::bridge::git_notes::NOTES_REF =>
                {
                    return git_overlay_note_ref_conflict_classification();
                }
                crate::bridge::git_core::GitBridgeError::NonFastForwardRef { name, .. } => {
                    if let Some(branch) = name.strip_prefix("refs/heads/") {
                        return git_overlay_remote_push_rejected_classification(branch);
                    }
                }
                crate::bridge::git_core::GitBridgeError::Conflict(message)
                    if is_git_overlay_mapping_conflict(message) =>
                {
                    return git_overlay_mapping_conflict_classification();
                }
                crate::bridge::git_core::GitBridgeError::GitHeddleThreadDiverged {
                    thread,
                    branch,
                    ..
                } => {
                    return git_heddle_thread_diverged_classification(thread, branch);
                }
                crate::bridge::git_core::GitBridgeError::RemoteDiverged {
                    branch,
                    upstream,
                    ..
                } => {
                    return git_overlay_remote_diverged_classification(branch, upstream);
                }
                crate::bridge::git_core::GitBridgeError::ShallowClone {
                    repository,
                    retry_command,
                } => {
                    return git_overlay_shallow_clone_classification(repository, retry_command);
                }
                _ => {}
            }
        }
        if let Some(heddle_err) = cause.downcast_ref::<HeddleError>() {
            match heddle_err {
                HeddleError::RepositoryNotFound(path) => {
                    let command =
                        format!("heddle init {}", shell_quote(&path.display().to_string()));
                    return ErrorClassification::known(
                        "repository_not_found",
                        format!("Run `{command}` to initialize the requested repository."),
                        format!("no Heddle repository was found at '{}'", path.display()),
                        "the command cannot inspect or change repository state until initialization",
                        "no repository objects, refs, metadata, or worktree files were changed",
                        command,
                    );
                }
                HeddleError::RepositoryExists(_) => {
                    return ErrorClassification::known(
                        "repository_exists",
                        "Run `heddle status` to inspect the existing repository.",
                        "a Heddle repository already exists at the requested path",
                        "initializing again could overwrite repository metadata",
                        "the existing repository was left unchanged",
                        "heddle status",
                    );
                }
                HeddleError::StateNotFound(_) => {
                    return ErrorClassification::known(
                        "state_not_found",
                        "List recent states with `heddle log`.",
                        "the requested state id does not exist in this repository",
                        "continuing with a guessed state could target the wrong history point",
                        "repository state, refs, metadata, and worktree files were left unchanged",
                        "heddle log",
                    );
                }
                HeddleError::InvalidObject(_)
                | HeddleError::Corruption { .. }
                | HeddleError::MissingObject { .. }
                | HeddleError::InvalidTreeEntry(_) => {
                    return ErrorClassification::known(
                        "repository_integrity_error",
                        "Inspect repository integrity with `heddle fsck --full`, then restore or repair the reported object/ref.",
                        "repository object or ref integrity did not pass validation",
                        "continuing could compound corruption or hide the missing object",
                        "the command stopped before applying the requested mutation",
                        "heddle fsck --full",
                    );
                }
                HeddleError::NotFound(message) if message == "No merge in progress" => {
                    return ErrorClassification::known(
                        "operation_not_in_progress",
                        "Run `heddle status` to see the current operation state.",
                        "there is no active merge operation to continue, abort, or inspect",
                        "continuing an absent operation could target unrelated work",
                        "repository state, refs, metadata, and worktree files were left unchanged",
                        "heddle status",
                    );
                }
                HeddleError::Io(io) => {
                    if objects::fs_atomic::is_out_of_space(io) {
                        return ErrorClassification::known(
                            "out_of_space",
                            "Free disk space and retry.",
                            "the filesystem reported no remaining space while Heddle was writing",
                            "retrying before freeing space may fail again or leave another partial write",
                            "atomic write boundaries preserved already-committed repository data",
                            "heddle status",
                        );
                    }
                    if objects::fs_atomic::is_permission_denied(io) {
                        return ErrorClassification::known(
                            "permission_denied",
                            "Check filesystem permissions on the repository directory.",
                            "the filesystem denied access to a path Heddle needed",
                            "retrying without permission changes will repeat the failed access",
                            "repository state was left at the last successful write boundary",
                            "heddle status",
                        );
                    }
                    if objects::fs_atomic::is_read_only_filesystem(io) {
                        return ErrorClassification::known(
                            "read_only_filesystem",
                            "Remount the filesystem read-write or move the repo to a writable path.",
                            "the repository path is on a read-only filesystem",
                            "mutating commands cannot persist repository state or worktree changes there",
                            "repository state was left at the last successful write boundary",
                            "heddle status",
                        );
                    }
                    if io.kind() == std::io::ErrorKind::NotFound {
                        return ErrorClassification::known(
                            "path_not_found",
                            "Check the --repo path, or create it and run `heddle init`.",
                            "the requested filesystem path does not exist",
                            "the command cannot inspect or change repository state at a missing path",
                            "no repository objects, refs, metadata, or worktree files were changed",
                            "heddle init",
                        );
                    }
                }
                _ => {}
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if objects::fs_atomic::is_out_of_space(io) {
                return ErrorClassification::known(
                    "out_of_space",
                    "Free disk space and retry.",
                    "the filesystem reported no remaining space while Heddle was writing",
                    "retrying before freeing space may fail again or leave another partial write",
                    "atomic write boundaries preserved already-committed repository data",
                    "heddle status",
                );
            }
            if objects::fs_atomic::is_permission_denied(io) {
                return ErrorClassification::known(
                    "permission_denied",
                    "Check filesystem permissions on the repository directory.",
                    "the filesystem denied access to a path Heddle needed",
                    "retrying without permission changes will repeat the failed access",
                    "repository state was left at the last successful write boundary",
                    "heddle status",
                );
            }
            if io.kind() == std::io::ErrorKind::NotFound {
                return ErrorClassification::known(
                    "path_not_found",
                    "Check the --repo path, or create it and run `heddle init`.",
                    "the requested filesystem path does not exist",
                    "the command cannot inspect or change repository state at a missing path",
                    "no repository objects, refs, metadata, or worktree files were changed",
                    "heddle init",
                );
            }
        }
    }
    // Fallback: string-shape matching for anyhow-only errors that don't carry
    // a downcastable `HeddleError` variant. The matches here are narrow on
    // purpose (anchored to the top of the displayed message), so they only
    // fire for the exact phrasings the CLI itself produces.
    let top = format!("{err:#}");
    if top.starts_with("State not found:") {
        return ErrorClassification::known(
            "state_not_found",
            "List recent states with `heddle log`.",
            "the requested state id does not exist in this repository",
            "continuing with a guessed state could target the wrong history point",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle log",
        );
    }
    if top.starts_with("Thread not found:") {
        return ErrorClassification::known(
            "thread_not_found",
            "List threads with `heddle thread list`.",
            "the requested thread id does not exist in this repository",
            "continuing with a guessed thread could target unrelated work",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle thread list",
        );
    }
    if top == "No merge in progress" || top.starts_with("object not found: No merge in progress") {
        return ErrorClassification::known(
            "operation_not_in_progress",
            "Run `heddle status` to see the current operation state.",
            "there is no active merge operation to continue, abort, or inspect",
            "continuing an absent operation could target unrelated work",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle status",
        );
    }
    if top == "No conflicts to resolve" {
        return ErrorClassification::known(
            "no_conflicts_to_resolve",
            "Run `heddle resolve --list` to inspect unresolved conflicts.",
            "there are no unresolved merge conflicts in the active operation",
            "marking nonexistent conflicts resolved would make operation state ambiguous",
            "repository state, refs, metadata, and worktree files were left unchanged",
            "heddle resolve --list",
        );
    }
    if top.starts_with("op_id_in_flight:") {
        return ErrorClassification::known(
            "op_id_in_flight",
            "Retry the same command after the in-flight operation completes.",
            "another process owns this operation id reservation",
            "running a second copy could duplicate a mutating operation",
            "no command body was executed for this retry",
            "heddle status",
        );
    }
    if top.starts_with("op_id_conflict:") {
        return ErrorClassification::known(
            "op_id_conflict",
            "Use the original command arguments with this --op-id, or generate a fresh op-id.",
            "the same operation id maps to a different request body",
            "reusing it for different arguments would make idempotent replay ambiguous",
            "no command body was executed for this retry",
            "heddle commands --output json",
        );
    }
    ErrorClassification::runtime()
}

fn is_git_overlay_mapping_conflict(message: &str) -> bool {
    (message.starts_with("git oid ") || message.starts_with("change id "))
        && message.contains(" mapped to ")
        && message.contains(" (new ")
}

fn git_overlay_note_ref_conflict_classification() -> ErrorClassification {
    let mut classification = ErrorClassification::known_with_error(
        "git_overlay_note_ref_conflict",
        "Remote Heddle notes do not fast-forward",
        "Fetch the remote Heddle notes, then retry the push. If the conflict remains, create a fresh Heddle clone from the remote so Git-to-Heddle identity metadata stays authoritative.",
        "updating refs/notes/heddle would replace remote Git-to-Heddle identity metadata instead of fast-forwarding it",
        "pushing would remap commits that another Heddle checkout already identified",
        "remote refs/notes/heddle was left unchanged",
        "heddle fetch",
    );
    classification.recovery_commands = vec![
        "heddle fetch".to_string(),
        "heddle push".to_string(),
        "heddle clone <remote> <fresh-path>".to_string(),
    ];
    classification
}

fn git_overlay_mapping_conflict_classification() -> ErrorClassification {
    ErrorClassification::known_with_error(
        "git_overlay_mapping_conflict",
        "Git-overlay mapping metadata disagrees with refs/notes/heddle",
        "The local sidecar and refs/notes/heddle disagree about Git-to-Heddle identity. Use a fresh Heddle clone from the remote, or restore the notes ref from the checkout whose mapping is authoritative before retrying.",
        "one Git commit maps to different Heddle change ids across the sidecar and refs/notes/heddle",
        "continuing would corrupt or hide the Git/Heddle identity mapping",
        "the command stopped before applying the requested ref or worktree update",
        "heddle clone <remote> <fresh-path>",
    )
}

fn git_overlay_shallow_clone_classification(
    repository: &std::path::Path,
    retry_command: &str,
) -> ErrorClassification {
    let primary_command = "heddle clone <remote> <fresh-path>".to_string();
    let mut classification = ErrorClassification::known_with_error(
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
    );
    classification.recovery_commands = vec![primary_command, retry_command.to_string()];
    classification
}

fn git_heddle_thread_diverged_classification(thread: &str, branch: &str) -> ErrorClassification {
    let primary_command = format!("heddle bridge git reconcile --ref {branch} --preview");
    let heddle_preview =
        format!("heddle bridge git reconcile --prefer heddle --ref {branch} --preview");
    let git_preview = format!("heddle bridge git reconcile --prefer git --ref {branch} --preview");
    let mut classification = ErrorClassification::known_with_error(
        "git_heddle_thread_diverged",
        "Git branch and Heddle thread have diverged",
        format!(
            "Inspect both local repair choices with `{primary_command}`. Preview mode does not move refs, update the index, change worktree files, push, or pull."
        ),
        format!(
            "Heddle thread '{thread}' and Git branch '{branch}' both contain history the other side lacks"
        ),
        "importing or syncing now would need to choose whether the local Git branch or Heddle thread is authoritative",
        "Heddle refs, Git refs, and worktree files were left unchanged",
        primary_command.clone(),
    );
    classification.recovery_commands = vec![primary_command, heddle_preview, git_preview];
    classification
}

fn git_overlay_remote_push_rejected_classification(branch: &str) -> ErrorClassification {
    let primary_command = "heddle fetch".to_string();
    let mut classification = ErrorClassification::known_with_error(
        "git_overlay_remote_diverged",
        "Remote branch does not fast-forward the local Git checkpoint",
        "Fetch first so Heddle can inspect the remote tip locally, then run `heddle verify` for the exact integration command.",
        format!(
            "pushing branch '{branch}' would rewrite the remote branch instead of fast-forwarding it"
        ),
        "pushing now would replace work that exists on the remote",
        "the remote branch, local Git branch, Heddle refs, index, and worktree files were left unchanged",
        primary_command.clone(),
    );
    classification.recovery_commands = vec![primary_command, "heddle verify".to_string()];
    classification
}

fn git_overlay_remote_diverged_classification(branch: &str, upstream: &str) -> ErrorClassification {
    let import_command = format!("heddle bridge git import --ref {upstream}");
    let merge_preview = format!("heddle merge {upstream} --preview");
    let mut classification = ErrorClassification::known_with_error(
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
    );
    classification.recovery_commands = vec![import_command, merge_preview];
    classification
}
