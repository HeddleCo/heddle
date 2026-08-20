// SPDX-License-Identifier: Apache-2.0
//! Closed-world checks for the top-level CLI and operation lifecycle.

use std::collections::BTreeSet;

use clap::Command;

use crate::cli::INIT_VERB;

/// Closed-world roots the current parser may expose. This is not the
/// heddle#473 destination (~23 everyday verbs; umbrella nouns do not
/// count as one; no `help advanced`).
///
/// `switch` and `clean` are reserved here even though the current parser
/// does not expose them at the root. Unreviewed roots still fail this
/// gate; reserved names stay listed so they cannot appear silently.
pub const CANONICAL_ROOT_COMMANDS: &[&str] = &[
    "abort",
    "adopt",
    "agent",
    "auth",
    "bridge",
    "clean",
    "clone",
    "commit",
    "context",
    "continue",
    "diff",
    "discuss",
    "doctor",
    "help",
    INIT_VERB,
    "integration",
    "land",
    "log",
    "maintenance",
    "presence",
    "pull",
    "push",
    "query",
    "ready",
    "redact",
    "remote",
    "resolve",
    "review",
    "run",
    "shell",
    "show",
    "start",
    "status",
    "switch",
    "sync",
    "thread",
    "try",
    "undo",
    "verify",
    "watch",
];

/// Explicitly reviewed non-everyday roots that exist in the current product.
///
/// This list is intentionally closed. Adding a new root requires updating the
/// design and this list in the same review; otherwise the source and
/// `doctor docs` conformance gates fail. `schemas` is retained as the audited
/// pre-existing Phase 2 regression and must not be mistaken for a canonical
/// root when that follow-up is handled. `completions` is the public script
/// verb from heddle#1439; `shell completion` remains the namespaced path.
pub const APPROVED_NON_EVERYDAY_ROOT_COMMANDS: &[&str] = &[
    "capture",
    "ci",
    "collapse",
    "complete",
    "completions",
    "daemon",
    "expand",
    "hook",
    "oplog",
    "retro",
    "revert",
    "schemas",
    "semantic",
    "timeline",
    "visibility",
    "whoami",
];

/// Reviewed root aliases. Keeping this set closed prevents a removed verb from
/// returning as a hidden clap alias and bypassing the root-variant check.
pub const APPROVED_ROOT_ALIASES: &[&str] = &["__complete", "history", "schema"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSurfaceViolation {
    pub path: Vec<String>,
    pub detail: String,
}

pub fn is_approved_root_command(command: &str) -> bool {
    CANONICAL_ROOT_COMMANDS.contains(&command)
        || APPROVED_NON_EVERYDAY_ROOT_COMMANDS.contains(&command)
}

/// Return every root that is outside the reviewed closed set.
pub fn unapproved_root_command_names<'a>(
    commands: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    commands
        .into_iter()
        .filter(|command| !is_approved_root_command(command))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Validate both parts of the Phase 5 close-the-class contract against a clap
/// tree: no unreviewed top-level root, and no command-local `--continue` or
/// `--abort` lifecycle flag.
pub fn command_surface_violations(command: &Command) -> Vec<CommandSurfaceViolation> {
    let mut violations = unapproved_root_command_names(
        command.get_subcommands().map(Command::get_name),
    )
    .into_iter()
    .map(|root| CommandSurfaceViolation {
        path: vec![root.clone()],
        detail: format!(
            "top-level verb `{root}` is outside the accepted canonical and reviewed advanced surface"
        ),
    })
    .collect::<Vec<_>>();

    for subcommand in command.get_subcommands() {
        for alias in subcommand.get_all_aliases() {
            if !APPROVED_ROOT_ALIASES.contains(&alias) {
                violations.push(CommandSurfaceViolation {
                    path: vec![alias.to_string()],
                    detail: format!(
                        "root alias `{alias}` is outside the explicitly reviewed alias surface"
                    ),
                });
            }
        }
    }

    let mut path = Vec::new();
    collect_lifecycle_flag_violations(command, &mut path, &mut violations);
    violations
}

fn collect_lifecycle_flag_violations(
    command: &Command,
    path: &mut Vec<String>,
    violations: &mut Vec<CommandSurfaceViolation>,
) {
    for subcommand in command.get_subcommands() {
        path.push(subcommand.get_name().to_string());
        for argument in subcommand.get_arguments() {
            let Some(flag) = argument.get_long() else {
                continue;
            };
            if matches!(flag, "continue" | "abort") {
                violations.push(CommandSurfaceViolation {
                    path: path.clone(),
                    detail: format!(
                        "`{}` owns a command-local `--{flag}` flag; use the operation-agnostic top-level `{flag}` verb",
                        path.join(" ")
                    ),
                });
            }
        }
        collect_lifecycle_flag_violations(subcommand, path, violations);
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use clap::{Arg, CommandFactory};

    use super::*;
    use crate::cli::Cli;

    #[test]
    fn current_cli_satisfies_closed_surface() {
        let violations = command_surface_violations(&Cli::command());
        assert!(
            violations.is_empty(),
            "current CLI violates the closed surface: {violations:#?}"
        );
    }

    #[test]
    fn reintroduced_noncanonical_root_fails_conformance() {
        let command = Cli::command().subcommand(Command::new("checkout"));
        let violations = command_surface_violations(&command);
        assert!(
            violations
                .iter()
                .any(|violation| violation.path == ["checkout"]),
            "reintroduced `checkout` must fail the closed-surface gate: {violations:#?}"
        );
    }

    #[test]
    fn command_local_lifecycle_flag_fails_conformance() {
        let command = Command::new("heddle")
            .subcommand(Command::new("resolve").arg(Arg::new("abort").long("abort")));
        let violations = command_surface_violations(&command);
        assert!(
            violations
                .iter()
                .any(|violation| violation.detail.contains("command-local `--abort`")),
            "command-local lifecycle flags must fail the gate: {violations:#?}"
        );
    }

    #[test]
    fn hidden_removed_root_alias_fails_conformance() {
        let command =
            Command::new("heddle").subcommand(Command::new("switch").hide(true).alias("checkout"));
        let violations = command_surface_violations(&command);
        assert!(
            violations
                .iter()
                .any(|violation| violation.path == ["checkout"]),
            "a hidden alias must not revive a removed root: {violations:#?}"
        );
    }
}
