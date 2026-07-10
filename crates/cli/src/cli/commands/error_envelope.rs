// SPDX-License-Identifier: Apache-2.0
//! Shared stderr error envelopes for CLI failures.

use clap::error::Error as ClapError;
use repo::Config;

use super::{
    RecoveryAdvice,
    command_catalog::{ActionTemplate, recommended_action_template, validate_recommended_action},
};
use crate::{
    cli::{Cli, render::shell_quote, should_output_json},
    exit::HeddleExitCode,
};

/// Print an error to stderr with a one-line next-step hint when the error
/// chain matches a known recoverable condition. Stays out of the way
/// otherwise — `anyhow`'s `Debug` impl is good enough for arbitrary errors.
///
/// Honours the resolved output format: when JSON is selected, emits a
/// single-line structured envelope instead of freeform text so scripts can
/// parse it cleanly. The envelope is a stderr-only contract — the stdout schemas in
/// `crates/cli/src/cli/commands/schemas.rs` are untouched.
pub fn print_error_with_hint(cli: &Cli, err: &anyhow::Error) {
    print_error_with_hint_inner(cli, err, None);
}

pub fn print_error_with_hint_with_config(cli: &Cli, err: &anyhow::Error, config: &Config) {
    print_error_with_hint_inner(cli, err, Some(config));
}

fn print_error_with_hint_inner(cli: &Cli, err: &anyhow::Error, config: Option<&Config>) {
    let verb_help = verb_specific_help_command(cli);
    let classification = classify_error_with_verb(err, verb_help.as_deref());
    let hint = classification.hint.clone();
    let kind = classification.kind.clone();
    let error = display_error_message(err, &kind);
    let json = should_output_json(cli, config);
    if json {
        let envelope_error = classification
            .human_error
            .as_deref()
            .unwrap_or(error.as_str());
        let primary_command_template = command_template(&classification.primary_command);
        let recovery_action_templates = command_templates(&classification.recovery_commands);
        let mut body = serde_json::json!({
            "error": envelope_error,
            "exit_code": HeddleExitCode::from_error(err).as_u8(),
            "hint": hint,
            "kind": kind,
            "unsafe_condition": classification.unsafe_condition,
            "would_change": classification.would_change,
            "preserved": classification.preserved,
            "primary_command": classification.primary_command,
            "primary_command_template": primary_command_template,
            "recovery_commands": classification.recovery_commands,
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
        // Always surface the rest of the typed recovery commands in
        // text mode. JSON callers got them in `recovery_commands`;
        // human readers shouldn't have to re-run with --output json
        // to discover the escape hatch (e.g. `--force` variants).
        if classification.recovery_commands.len() > 1 {
            eprintln!("Also: {}", classification.recovery_commands[1..].join(", "));
        }
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
        } else if cli.verbose > 0 {
            eprintln!("Unsafe: {}", classification.unsafe_condition);
            eprintln!("Would change: {}", classification.would_change);
            eprintln!("Preserved: {}", classification.preserved);
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
    let primary_command = "heddle help --output json";
    let recovery_commands = vec![
        primary_command.to_string(),
        "heddle help --output text".to_string(),
    ];
    let recovery_action_templates = command_templates(&recovery_commands);
    let body = serde_json::json!({
        "error": err.to_string(),
        "exit_code": HeddleExitCode::from_clap(err).as_u8(),
        "hint": "Run `heddle help --output json` to inspect the command surface.",
        "kind": "parse_error",
        "unsafe_condition": "the requested arguments do not match the registered command surface",
        "would_change": "the command body was not executed, so no repository state could be changed",
        "preserved": "no command body was executed",
        "primary_command": primary_command,
        "primary_command_template": command_template(primary_command),
        "recovery_commands": recovery_commands,
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
        "operation_not_in_progress" | "no_merge_in_progress" => "No merge in progress".to_string(),
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
        Self::from_recovery_fields(
            advice.kind,
            Some(advice.error.clone()),
            advice.primary_hint().to_string(),
            advice.unsafe_condition.clone(),
            advice.would_change.clone(),
            advice.preserved.clone(),
            advice.primary_command.clone(),
            advice.recovery_commands.clone(),
            advice.extra_json_fields.clone(),
        )
    }

    fn from_recovery_details(details: &objects::RecoveryDetails) -> Self {
        // Prefer explicit, path-specific commands the callsite attached (e.g.
        // `heddle --repo <checkout> ready …` for source-thread refusals). The
        // `kind`-keyed fallback below has no access to that path, so it can only
        // reconstruct the generic recovery variant (HeddleCo/heddle#981).
        let recovery_commands = details
            .recovery_commands
            .clone()
            .filter(|commands| !commands.is_empty())
            .unwrap_or_else(|| typed_recovery_commands(details.kind));
        let primary_command = recovery_commands
            .first()
            .cloned()
            .unwrap_or_else(|| "heddle help --output json".to_string());
        Self::from_recovery_fields(
            details.kind,
            Some(details.error.clone()),
            details.hint.clone(),
            details.unsafe_condition.clone(),
            details.would_change.clone(),
            details.preserved.clone(),
            primary_command,
            recovery_commands,
            serde_json::Map::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_recovery_fields(
        kind: &str,
        human_error: Option<String>,
        hint: String,
        unsafe_condition: String,
        would_change: String,
        preserved: String,
        primary_command: String,
        recovery_commands: Vec<String>,
        mut extra_json_fields: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let validation = AdviceActionValidation::new(&primary_command, &recovery_commands);
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
                serde_json::Value::String(primary_command),
            );
        }
        Self {
            kind: kind.to_string(),
            human_error,
            hint,
            unsafe_condition,
            would_change,
            preserved,
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

fn typed_recovery_commands(kind: &str) -> Vec<String> {
    let commands: &[&str] = match kind {
        "state_corrupted" => &["heddle verify", "heddle fsck --full"],
        "repository_integrity_error" => &["heddle fsck --full"],
        "repository_not_found" => &["heddle init"],
        "state_not_found" => &["heddle log"],
        // Merge-orchestration refusals raised from core as typed
        // `RecoveryDetails` (crates/core/src/merge/advice.rs). Before this
        // mapping they all degraded to `heddle help --output json` in the
        // machine envelope, losing the specific recovery path the human
        // hint already documents (HeddleCo/heddle#981 regression). Commands
        // mirror the CLI-side `RecoveryAdvice` versions on `main`.
        "merge_already_in_progress" => {
            &["heddle status", "heddle continue", "heddle resolve --abort"]
        }
        "thread_not_found" => &["heddle thread list"],
        "merge_no_common_ancestor" => &["heddle status"],
        // Generic capture/stash recovery. `source_thread_uncaptured_work`
        // normally arrives with explicit path-specific `heddle --repo
        // <checkout> ...` commands attached to `RecoveryDetails`
        // (`from_recovery_details` prefers those); this `kind`-keyed fallback
        // only applies when no explicit commands were set. `dirty_worktree` has
        // no checkout path to scope to, so the generic form matches `main`.
        "dirty_worktree" | "source_thread_uncaptured_work" => &[
            super::advice::DIRTY_WORKTREE_COMMIT_COMMAND,
            super::advice::DIRTY_WORKTREE_CAPTURE_COMMAND,
            super::advice::DIRTY_WORKTREE_STASH_COMMAND,
        ],
        _ => &["heddle help --output json"],
    };
    commands
        .iter()
        .map(|command| (*command).to_string())
        .collect()
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

        let fallback = "heddle help --output json".to_string();
        valid_recovery_commands.retain(|command| command != &fallback);
        valid_recovery_commands.insert(0, fallback.clone());
        Self {
            primary_command: fallback,
            recovery_commands: valid_recovery_commands,
            violations,
        }
    }
}

fn command_template(command: &str) -> Option<ActionTemplate> {
    recommended_action_template(command)
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
/// Build the verb-specific help command for the active CLI invocation,
/// or `None` if no concrete verb path resolves. Used as a per-call
/// fallback recovery template so even untyped error envelopes ship a
/// "where to learn more" entry tied to the command that just failed
/// — replacing the historical bare `heddle status` hint for everything.
fn verb_specific_help_command(cli: &Cli) -> Option<String> {
    let path = crate::cli::commands::command_catalog::command_path(&cli.command);
    if path.is_empty() {
        return None;
    }
    Some(format!("heddle help {}", path.join(" ")))
}

#[cfg(test)]
fn classify_error(err: &anyhow::Error) -> ErrorClassification {
    classify_error_with_verb(err, None)
}

fn classify_error_with_verb(err: &anyhow::Error, verb_help: Option<&str>) -> ErrorClassification {
    let mut classification = classify_error_inner(err);
    if classification.kind == "runtime_error"
        && let Some(help) = verb_help
        && !classification.recovery_commands.iter().any(|c| c == help)
    {
        classification.recovery_commands.push(help.to_string());
    }
    classification
}

fn classify_error_inner(err: &anyhow::Error) -> ErrorClassification {
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
        if let Some(git_error) =
            cause.downcast_ref::<crate::git_projection_engine::git_core::GitProjectionError>()
            && let Some(advice) = RecoveryAdvice::from_git_projection_error(git_error)
        {
            return ErrorClassification::from_advice(&advice);
        }
        if let Some(heddle_err) = cause.downcast_ref::<HeddleError>() {
            if let HeddleError::Recovery(details) = heddle_err {
                return ErrorClassification::from_recovery_details(details);
            }
            if let HeddleError::ConfigInvalidValue {
                path,
                key,
                value,
                valid_values,
            } = heddle_err
            {
                let path_display = path.display().to_string();
                let valid = valid_values.join(" or ");
                return ErrorClassification {
                    kind: "invalid_repo_config_output_format".to_string(),
                    human_error: Some(format!(
                        "invalid {key}: '{value}' — valid values are {valid} (in {path_display})"
                    )),
                    hint: format!(
                        "Edit {path_display} and set {key} to {}.",
                        valid_values.join(" or ")
                    ),
                    unsafe_condition: format!(
                        "configuration at {path_display} declares an unknown {key} value"
                    ),
                    would_change:
                        "the requested command did not run because Heddle could not load the configuration"
                            .to_string(),
                    preserved:
                        "no repository objects, refs, metadata, or worktree files were changed"
                            .to_string(),
                    primary_command: "heddle status".to_string(),
                    recovery_commands: vec!["heddle status".to_string()],
                    extra_json_fields: serde_json::Map::new(),
                };
            }
            match heddle_err {
                // Corrupted stored state (HeddleCo/heddle#642): decode
                // failures must surface as a recovery path, not raw msgpack
                // internals — `heddle status` is the natural recovery probe
                // and would otherwise dead-end on the same opaque error.
                HeddleError::Serialization(detail) => {
                    return ErrorClassification::from_recovery_details(
                        &objects::RecoveryDetails::serialization_error(detail),
                    );
                }
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
                HeddleError::RepositoryFormatTooNew {
                    found, supported, ..
                } => {
                    return ErrorClassification {
                        kind: "repository_format_too_new".to_string(),
                        human_error: Some(heddle_err.to_string()),
                        hint:
                            "Upgrade heddle to a binary that supports this repository format, or run the repository migration command with a compatible binary."
                                .to_string(),
                        unsafe_condition: format!(
                            "repository format {found} is newer than this binary's supported format {supported}"
                        ),
                        would_change:
                            "opening a newer-format repository could misread unstamped on-disk data"
                                .to_string(),
                        preserved:
                            "no repository objects, refs, metadata, or worktree files were changed"
                                .to_string(),
                        primary_command: "heddle status".to_string(),
                        recovery_commands: vec!["heddle status".to_string()],
                        extra_json_fields: serde_json::Map::new(),
                    };
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
                HeddleError::StateNotFound(state_id) => {
                    return ErrorClassification::from_recovery_details(
                        &objects::RecoveryDetails::state_not_found(state_id),
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
                HeddleError::NoMergeInProgress => {
                    return ErrorClassification::known(
                        "no_merge_in_progress",
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
    ErrorClassification::runtime()
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use objects::{HeddleError, RecoveryDetails};

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
        assert_eq!(classified.primary_command, "heddle help --output json");
        assert_eq!(
            classified.recovery_commands,
            vec!["heddle help --output json"]
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

    #[test]
    fn typed_recovery_error_classifies_like_recovery_advice() {
        let err = anyhow!(HeddleError::recovery(RecoveryDetails::serialization_error(
            "bad marker"
        )));

        let classified = classify_error(&err);
        assert_eq!(classified.kind, "state_corrupted");
        assert_eq!(
            classified.human_error.as_deref(),
            Some("Repository state is corrupted or unreadable")
        );
        assert_eq!(classified.primary_command, "heddle verify");
        assert_eq!(
            classified.recovery_commands,
            vec!["heddle verify", "heddle fsck --full"]
        );
        assert!(classified.extra_json_fields.is_empty());
    }

    #[test]
    fn typed_recovery_error_with_unknown_kind_uses_help_fallback() {
        let err = anyhow!(HeddleError::recovery(RecoveryDetails::safety_refusal(
            "bad_typed_recovery_fixture",
            "bad advice",
            "bad hint",
            "unsafe",
            "would change",
            "nothing changed",
        )));

        let classified = classify_error(&err);
        assert_eq!(classified.kind, "bad_typed_recovery_fixture");
        assert_eq!(classified.primary_command, "heddle help --output json");
        assert_eq!(
            classified.recovery_commands,
            vec!["heddle help --output json"]
        );
        assert!(classified.extra_json_fields.is_empty());
    }

    #[test]
    fn typed_invalid_output_format_classifies_without_toml_message_matching() {
        let err = anyhow!(HeddleError::ConfigInvalidValue {
            path: std::path::PathBuf::from("/tmp/heddle-config.toml"),
            key: "output.format".to_string(),
            value: "auto".to_string(),
            valid_values: vec!["'text'".to_string(), "'json'".to_string()],
        });

        let classified = classify_error(&err);
        assert_eq!(classified.kind, "invalid_repo_config_output_format");
        assert_eq!(classified.primary_command, "heddle status");
        assert!(
            classified
                .human_error
                .as_deref()
                .is_some_and(|error| error.contains("output.format") && error.contains("'auto'"))
        );
    }

    #[test]
    fn typed_no_merge_in_progress_gets_operation_recovery() {
        let err = anyhow!(HeddleError::NoMergeInProgress);

        let classified = classify_error(&err);
        assert_eq!(classified.kind, "no_merge_in_progress");
        assert_eq!(classified.primary_command, "heddle status");
        assert!(classified.unsafe_condition.contains("no active merge"));
    }

    #[test]
    fn typed_merge_refusal_kinds_keep_specific_recovery_commands() {
        // HeddleCo/heddle#981: merge-orchestration refusals raised from core
        // as typed `RecoveryDetails` must not degrade to
        // `heddle help --output json` in the machine envelope.
        let in_progress = anyhow!(HeddleError::recovery(RecoveryDetails::safety_refusal(
            "merge_already_in_progress",
            "A merge is already in progress",
            "hint",
            "unsafe",
            "would change",
            "preserved",
        )));
        let classified = classify_error(&in_progress);
        assert_eq!(classified.kind, "merge_already_in_progress");
        assert_eq!(classified.primary_command, "heddle status");
        assert_eq!(
            classified.recovery_commands,
            vec!["heddle status", "heddle continue", "heddle resolve --abort"]
        );

        let not_found = anyhow!(HeddleError::recovery(RecoveryDetails::safety_refusal(
            "thread_not_found",
            "Thread 'x' not found",
            "hint",
            "unsafe",
            "would change",
            "preserved",
        )));
        let classified = classify_error(&not_found);
        assert_eq!(classified.primary_command, "heddle thread list");

        let dirty = anyhow!(HeddleError::recovery(RecoveryDetails::safety_refusal(
            "dirty_worktree",
            "Refusing to merge with a dirty worktree",
            "hint",
            "unsafe",
            "would change",
            "preserved",
        )));
        let classified = classify_error(&dirty);
        assert_ne!(classified.primary_command, "heddle help --output json");
        assert_eq!(classified.recovery_commands.len(), 3);
    }

    #[test]
    fn typed_state_not_found_routes_through_recovery_details() {
        let state = objects::object::ChangeId::generate();
        let err = anyhow!(HeddleError::StateNotFound(state));

        let classified = classify_error(&err);
        assert_eq!(classified.kind, "state_not_found");
        assert_eq!(classified.primary_command, "heddle log");
        assert_eq!(classified.recovery_commands, vec!["heddle log"]);
    }
}
