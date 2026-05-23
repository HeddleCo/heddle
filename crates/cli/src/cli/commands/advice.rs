// SPDX-License-Identifier: Apache-2.0
//! Typed refusal and recovery advice shared by command surfaces.

use std::{error::Error, fmt};

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
        }
    }

    pub(crate) fn dirty_worktree(
        action: &str,
        dirty_paths: Vec<String>,
        already_preserved: impl Into<String>,
    ) -> Self {
        let path_summary = if dirty_paths.is_empty() {
            "uncommitted paths were detected".to_string()
        } else {
            format!(
                "modified, deleted, or untracked path(s): {}",
                dirty_paths
                    .iter()
                    .take(12)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let overflow = dirty_paths.len().saturating_sub(12);
        let unsafe_condition = if overflow == 0 {
            path_summary
        } else {
            format!("{path_summary}, and {overflow} more")
        };
        let primary_command = "heddle capture -m \"...\"".to_string();
        Self {
            kind: "dirty_worktree",
            error: format!("Refusing to {action}: worktree has uncommitted changes"),
            hint: format!(
                "Preserve the work with `{primary_command}` or `heddle stash push -m \"...\"`, then retry."
            ),
            unsafe_condition,
            would_change: format!(
                "{action} would write a different tree into the worktree and could discard those paths"
            ),
            preserved: already_preserved.into(),
            primary_command,
            recovery_commands: vec![
                "heddle capture -m \"...\"".to_string(),
                "heddle stash push -m \"...\"".to_string(),
            ],
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
            recovery_commands.extend([
                "heddle capture -m \"...\"".to_string(),
                "heddle stash push -m \"...\"".to_string(),
            ]);
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
        }
    }

    pub(crate) fn op_id_conflict() -> Self {
        Self {
            kind: "op_id_conflict",
            error: "--op-id was already used with different arguments".to_string(),
            hint: "Retry with the original arguments for this --op-id or generate a fresh operation id.".to_string(),
            unsafe_condition: "the same operation id maps to a different request body".to_string(),
            would_change: "reusing it for different arguments would make idempotent replay ambiguous".to_string(),
            preserved: "no command body was executed for this retry".to_string(),
            primary_command: "heddle commands --output json".to_string(),
            recovery_commands: vec!["heddle commands --output json".to_string()],
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
        }
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

    pub fn primary_hint(&self) -> &str {
        &self.hint
    }
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
