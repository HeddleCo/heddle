// SPDX-License-Identifier: Apache-2.0
//! Shared stderr error envelopes for CLI failures.

use clap::error::Error as ClapError;

use super::{
    RecoveryAdvice,
    command_catalog::{ActionTemplate, recommended_action_template, validate_recommended_action},
    recommended_action_argv,
};
use crate::cli::{Cli, render::shell_quote, should_output_json};
use crate::exit::HeddleExitCode;

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
            "exit_code": HeddleExitCode::from_error(err).as_u8(),
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
        "exit_code": HeddleExitCode::from_clap(err).as_u8(),
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
        let validation =
            AdviceActionValidation::new(&advice.primary_command, &advice.recovery_commands);
        let mut extra_json_fields = advice.extra_json_fields.clone();
        if !validation.violations.is_empty() {
            extra_json_fields.insert(
                "advice_contract_valid".to_string(),
                serde_json::Value::Bool(false),
            );
            extra_json_fields.insert(
                "advice_contract_violations".to_string(),
                serde_json::Value::Array(
                    validation
                        .violations
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            extra_json_fields.insert(
                "original_primary_command".to_string(),
                serde_json::Value::String(advice.primary_command.clone()),
            );
        }
        Self {
            kind: advice.kind.to_string(),
            human_error: Some(advice.error.clone()),
            hint: advice.primary_hint().to_string(),
            unsafe_condition: advice.unsafe_condition.clone(),
            would_change: advice.would_change.clone(),
            preserved: advice.preserved.clone(),
            primary_command: validation.primary_command,
            recovery_commands: validation.recovery_commands,
            extra_json_fields,
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

struct AdviceActionValidation {
    primary_command: String,
    recovery_commands: Vec<String>,
    violations: Vec<String>,
}

impl AdviceActionValidation {
    fn new(primary_command: &str, recovery_commands: &[String]) -> Self {
        let mut violations = Vec::new();
        if let Err(error) = validate_recommended_action(primary_command) {
            violations.push(format!(
                "primary_command `{primary_command}` is not a registered action: {error}"
            ));
        }

        let mut valid_recovery_commands = Vec::new();
        for command in recovery_commands {
            match validate_recommended_action(command) {
                Ok(()) => valid_recovery_commands.push(command.clone()),
                Err(error) => violations.push(format!(
                    "recovery_command `{command}` is not a registered action: {error}"
                )),
            }
        }

        if violations.is_empty() {
            return Self {
                primary_command: primary_command.to_string(),
                recovery_commands: if recovery_commands.is_empty() {
                    vec![primary_command.to_string()]
                } else {
                    recovery_commands.to_vec()
                },
                violations,
            };
        }

        let fallback = "heddle commands --output json".to_string();
        valid_recovery_commands.retain(|command| command != &fallback);
        valid_recovery_commands.insert(0, fallback.clone());
        Self {
            primary_command: fallback,
            recovery_commands: valid_recovery_commands,
            violations,
        }
    }
}

fn command_argv(command: &str) -> Option<Vec<String>> {
    match recommended_action_argv(command) {
        Ok(argv) => argv,
        Err(error) => {
            debug_assert!(
                false,
                "invalid command action reached error envelope: {command}: {error}"
            );
            None
        }
    }
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
            if let Some(advice) = RecoveryAdvice::from_git_bridge_error(git_error) {
                return ErrorClassification::from_advice(&advice);
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

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::{RecoveryAdvice, classify_error};

    #[test]
    fn recovery_advice_with_invalid_actions_falls_back_to_contract_catalog() {
        let err = anyhow!(RecoveryAdvice::safety_refusal(
            "bad_advice_fixture",
            "bad advice",
            "bad hint",
            "unsafe",
            "would change",
            "nothing changed",
            "git status",
            vec!["git status".to_string()],
        ));

        let classified = classify_error(&err);
        assert_eq!(classified.kind, "bad_advice_fixture");
        assert_eq!(classified.primary_command, "heddle commands --output json");
        assert_eq!(
            classified.recovery_commands,
            vec!["heddle commands --output json"]
        );
        assert_eq!(
            classified.extra_json_fields["advice_contract_valid"],
            serde_json::Value::Bool(false)
        );
        assert!(
            classified
                .extra_json_fields
                .get("advice_contract_violations")
                .and_then(|value| value.as_array())
                .is_some_and(|violations| violations.len() == 2)
        );
    }
}
