// SPDX-License-Identifier: Apache-2.0
//! Per-surface outcome and bounded recovery for collaboration a push could
//! not complete (heddle#1904).
//!
//! The source publication and each collaboration surface succeed or fail on
//! their own. For every record the remote does not hold afterwards, the push
//! says whether it is still unsent or stays local, whether resending the same
//! signed command could help, which operation IDs it carries, and the ordered
//! commands that recover it. JSON and human output render the same facts.

use heddle_cli_contract::cli::commands::{
    command_catalog::{ActionFields, recommended_action_template},
    wire::remote::{PushOutput, PushReplicationItem, PushReplicationOutcome},
};
use hosted_client::client::replication_report::{
    ReplicationIssue, ReplicationIssueKind, ReplicationReport, classify_error,
};

use crate::{
    cli::{
        commands::{
            action_line::print_next_exact,
            next_action::{NextActionValidationContext, validate_next_action},
        },
        render::shell_quote,
        style,
    },
    exit::HeddleExitCode,
};

/// A collaboration surface a hosted push replicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Surface {
    Discussions,
    Context,
}

impl Surface {
    fn label(self) -> &'static str {
        match self {
            Self::Discussions => "discussions",
            Self::Context => "context",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Discussions => "discussion",
            Self::Context => "context",
        }
    }

    /// Inspect one record; with no record, list the surface.
    fn inspect(self, record: Option<&str>) -> String {
        match (self, record) {
            (Self::Discussions, Some(id)) => format!("heddle discuss show {}", shell_quote(id)),
            (Self::Context, Some(id)) => format!("heddle context history {}", shell_quote(id)),
            (Self::Discussions, None) => "heddle discuss list".to_string(),
            (Self::Context, None) => "heddle context list".to_string(),
        }
    }
}

/// Where the push went, so recovery commands repeat it exactly.
#[derive(Debug, Clone, Copy)]
pub(super) struct RecoveryTarget<'a> {
    pub(super) remote: Option<&'a str>,
    pub(super) thread: &'a str,
}

impl RecoveryTarget<'_> {
    fn push(&self) -> String {
        let thread = shell_quote(self.thread);
        match self.remote {
            Some(remote) => format!("heddle push {} --thread {thread}", shell_quote(remote)),
            None => format!("heddle push --thread {thread}"),
        }
    }

    fn pull(&self) -> String {
        match self.remote {
            Some(remote) => format!("heddle pull {}", shell_quote(remote)),
            None => "heddle pull".to_string(),
        }
    }
}

/// Project one surface's replication result.
pub(super) fn surface_outcome(
    surface: Surface,
    result: &anyhow::Result<ReplicationReport>,
    target: RecoveryTarget<'_>,
) -> PushReplicationOutcome {
    match result {
        Ok(report) => {
            let mut outcome = PushReplicationOutcome::with_status(if report.complete() {
                "succeeded"
            } else {
                "failed"
            });
            outcome.count = Some(report.accepted);
            let blocking: Vec<_> = report
                .issues
                .iter()
                .filter(|issue| issue.kind.blocks_completion())
                .map(|issue| {
                    format!(
                        "{}: {}",
                        issue.record_id.as_deref().unwrap_or(surface.label()),
                        issue.message
                    )
                })
                .collect();
            if !blocking.is_empty() {
                outcome.error = Some(format!(
                    "hosted {} replication incomplete: {}",
                    surface.noun(),
                    blocking.join("; ")
                ));
            }
            for issue in &report.issues {
                place(&mut outcome, item(surface, issue, target));
            }
            outcome
        }
        Err(error) => {
            let mut outcome = PushReplicationOutcome::with_status("failed");
            outcome.error = Some(format!("{error:#}"));
            let issue = ReplicationIssue {
                record_id: None,
                revision_id: None,
                kind: classify_error(error),
                command: None,
                message: format!("{error:#}"),
            };
            place(&mut outcome, item(surface, &issue, target));
            outcome
        }
    }
}

fn place(outcome: &mut PushReplicationOutcome, item: PushReplicationItem) {
    if item.kind == ReplicationIssueKind::Superseded.as_str()
        || item.kind == ReplicationIssueKind::NotReplicable.as_str()
    {
        outcome.local_only.push(item);
    } else {
        outcome.unsent.push(item);
    }
}

fn item(
    surface: Surface,
    issue: &ReplicationIssue,
    target: RecoveryTarget<'_>,
) -> PushReplicationItem {
    let (guidance, recovery_commands) = recovery(surface, issue, target);
    PushReplicationItem {
        record_id: issue.record_id.clone(),
        revision_id: issue.revision_id.clone(),
        kind: issue.kind.as_str(),
        retry_unchanged: issue.kind.retry_unchanged(),
        client_operation_id: issue
            .command
            .as_ref()
            .map(|command| command.client_operation_id.clone()),
        signed_operation_id: issue
            .command
            .as_ref()
            .map(|command| command.signed_operation_id.clone()),
        message: issue.message.clone(),
        guidance: if issue.kind.local_only() {
            guidance.to_string()
        } else {
            format!("Unsent work stays local. {guidance}")
        },
        recovery_action_templates: recovery_commands
            .iter()
            .filter_map(|command| recommended_action_template(command))
            .collect(),
        recovery_commands,
    }
}

/// The guidance and ordered, bounded commands that recover one record.
fn recovery(
    surface: Surface,
    issue: &ReplicationIssue,
    target: RecoveryTarget<'_>,
) -> (&'static str, Vec<String>) {
    let record = issue.record_id.as_deref();
    match issue.kind {
        ReplicationIssueKind::StaleVersion => match (surface, record) {
            (Surface::Context, Some(id)) => (
                "The hosted annotation changed after this revision was prepared. Resending it unchanged cannot succeed, so it is not resent: refresh, compare the history, then author an explicit revision.",
                vec![
                    target.pull(),
                    surface.inspect(record),
                    format!("heddle context edit {} --body <text>", shell_quote(id)),
                ],
            ),
            _ => (
                "The hosted record changed after this command was prepared. Resending it unchanged cannot succeed: refresh and compare before acting on it again.",
                vec![target.pull(), surface.inspect(record)],
            ),
        },
        ReplicationIssueKind::InvalidCommand => (
            "The remote refused this signed command itself. Resending it unchanged cannot succeed; it is kept, with its operation IDs, for inspection.",
            vec![surface.inspect(record)],
        ),
        ReplicationIssueKind::Transient => (
            "Delivery did not complete. Push again: the identical signed command is resent under the same operation IDs and applied at most once.",
            vec![target.push()],
        ),
        ReplicationIssueKind::PermissionDenied => (
            "The signed-in identity lacks authority for this command. Check it, then push again.",
            vec!["heddle auth status".to_string(), target.push()],
        ),
        ReplicationIssueKind::NotReplicable => (
            "No hosted command can carry this local record, so nothing was sent; it stays local.",
            vec![surface.inspect(record)],
        ),
        ReplicationIssueKind::Superseded => (
            "Refused as stale and replaced by an explicit revision; it stays in local history and is never sent.",
            Vec::new(),
        ),
    }
}

/// Point the push's next action at the first recovery step of the first
/// unfinished collaboration surface, so an agent reading only `next_action`
/// is not told the push needs nothing. A step that would re-run `push` is
/// not a next action (the self-loop rule); a retry-safe failure says so
/// through `retry_unchanged` and exit code 75 instead.
pub(super) fn apply_next_action(output: &mut PushOutput) {
    let Some(first) = [&output.discussions, &output.context]
        .into_iter()
        .flat_map(|surface| surface.unsent.iter().chain(&surface.local_only))
        .flat_map(|item| &item.recovery_commands)
        .find(|command| {
            validate_next_action(
                command,
                NextActionValidationContext::without_repo(&["push"]),
            )
            .is_ok()
        })
    else {
        return;
    };
    let action = ActionFields::from_action(first);
    output.next_action = action.action.clone();
    output.next_action_template = action.template.clone();
    output.recommended_action = action.action;
    output.recommended_action_template = action.template;
}

/// Exit code for an incomplete push: 75 (safe to retry with the same
/// arguments) only when every unfinished record can be resent unchanged;
/// otherwise 65, because rerunning the same push cannot finish it.
pub(super) fn incomplete_exit(surfaces: &[&PushReplicationOutcome]) -> HeddleExitCode {
    let mut blocking = surfaces
        .iter()
        .flat_map(|surface| surface.unsent.iter().chain(&surface.local_only))
        .filter(|item| item.kind != ReplicationIssueKind::Superseded.as_str())
        .peekable();
    if blocking.peek().is_some() && blocking.all(|item| item.retry_unchanged) {
        HeddleExitCode::TempFail
    } else {
        HeddleExitCode::DataErr
    }
}

/// Render the same per-surface facts `--output json` carries.
pub(super) fn print_human(output: &PushOutput) {
    let partial = output.outcome.status == "partial";
    if partial {
        println!("source: published");
    }
    for (surface, outcome) in [
        (Surface::Discussions, &output.discussions),
        (Surface::Context, &output.context),
    ] {
        if !partial && outcome.unsent.is_empty() && outcome.local_only.is_empty() {
            continue;
        }
        let marker = if outcome.status == "succeeded" {
            style::ok_marker()
        } else {
            style::warn_marker()
        };
        println!(
            "{marker} {}: {}; {} accepted this push, {} unsent, {} local-only",
            surface.label(),
            if outcome.status == "succeeded" {
                "published"
            } else {
                "incomplete"
            },
            outcome.count.unwrap_or(0),
            outcome.unsent.len(),
            outcome.local_only.len()
        );
        for item in outcome.unsent.iter().chain(&outcome.local_only) {
            println!(
                "  {} {}{}",
                item.record_id.as_deref().unwrap_or(surface.label()),
                item.kind,
                item.revision_id
                    .as_deref()
                    .map(|revision| format!(" (revision {revision})"))
                    .unwrap_or_default()
            );
            if let (Some(client), Some(signed)) = (
                item.client_operation_id.as_deref(),
                item.signed_operation_id.as_deref(),
            ) {
                println!(
                    "    {}",
                    style::dim(&format!(
                        "client operation {client}, signed operation {signed}"
                    ))
                );
            }
            println!("    {}", item.guidance);
            for command in &item.recovery_commands {
                println!("    recovery: {command}");
            }
        }
    }
    if let Some(next) = output.next_action.as_deref()
        && collaboration_recovery_pending(output)
    {
        // The exact command, as in JSON: abbreviated IDs would not run.
        print_next_exact(next);
    }
}

/// Whether the next action comes from a collaboration recovery step.
fn collaboration_recovery_pending(output: &PushOutput) -> bool {
    [&output.discussions, &output.context]
        .into_iter()
        .flat_map(|surface| surface.unsent.iter().chain(&surface.local_only))
        .any(|item| !item.recovery_commands.is_empty())
}

#[cfg(test)]
mod tests {
    use hosted_client::client::replication_report::CommandIds;

    use super::*;

    fn issue(kind: ReplicationIssueKind) -> ReplicationIssue {
        ReplicationIssue {
            record_id: Some("01a0f01a-cb64-724e-a8f5-4c9753c3371c".into()),
            revision_id: None,
            kind,
            command: Some(CommandIds {
                client_operation_id: "c1".into(),
                signed_operation_id: "s1".into(),
            }),
            message: "refused".into(),
        }
    }

    const TARGET: RecoveryTarget<'static> = RecoveryTarget {
        remote: Some("origin"),
        thread: "feature/x",
    };

    #[test]
    fn every_recovery_command_is_a_valid_action() {
        for kind in [
            ReplicationIssueKind::StaleVersion,
            ReplicationIssueKind::InvalidCommand,
            ReplicationIssueKind::Transient,
            ReplicationIssueKind::PermissionDenied,
            ReplicationIssueKind::NotReplicable,
            ReplicationIssueKind::Superseded,
        ] {
            for surface in [Surface::Discussions, Surface::Context] {
                for record in [
                    issue(kind),
                    ReplicationIssue {
                        record_id: None,
                        ..issue(kind)
                    },
                ] {
                    let item = item(surface, &record, TARGET);
                    assert_eq!(
                        item.recovery_action_templates.len(),
                        item.recovery_commands.len(),
                        "{kind:?} {surface:?}: every step has a template"
                    );
                    for command in &item.recovery_commands {
                        heddle_cli_contract::cli::commands::command_catalog::validate_recommended_action(command)
                            .unwrap_or_else(|error| panic!("{command}: {error}"));
                    }
                    assert_eq!(
                        item.retry_unchanged,
                        kind == ReplicationIssueKind::Transient
                    );
                    assert!(!item.guidance.is_empty());
                }
            }
        }
    }

    #[test]
    fn stale_context_recovery_is_refresh_compare_then_explicit_revision() {
        let item = item(
            Surface::Context,
            &issue(ReplicationIssueKind::StaleVersion),
            TARGET,
        );
        assert_eq!(
            item.recovery_commands,
            vec![
                "heddle pull origin",
                "heddle context history 01a0f01a-cb64-724e-a8f5-4c9753c3371c",
                "heddle context edit 01a0f01a-cb64-724e-a8f5-4c9753c3371c --body <text>",
            ]
        );
        assert!(!item.retry_unchanged);
        assert_eq!(item.client_operation_id.as_deref(), Some("c1"));
        assert_eq!(item.signed_operation_id.as_deref(), Some("s1"));
        let transient = super::item(
            Surface::Context,
            &issue(ReplicationIssueKind::Transient),
            TARGET,
        );
        assert_eq!(
            transient.recovery_commands,
            vec!["heddle push origin --thread feature/x"]
        );
    }

    #[test]
    fn only_retry_safe_failures_exit_tempfail() {
        let outcome = |kinds: &[ReplicationIssueKind]| {
            surface_outcome(
                Surface::Context,
                &Ok(ReplicationReport {
                    accepted: 0,
                    issues: kinds.iter().copied().map(issue).collect(),
                }),
                TARGET,
            )
        };
        let transient = outcome(&[ReplicationIssueKind::Transient]);
        assert_eq!(incomplete_exit(&[&transient]), HeddleExitCode::TempFail);
        let stale = outcome(&[
            ReplicationIssueKind::Transient,
            ReplicationIssueKind::StaleVersion,
        ]);
        assert_eq!(
            incomplete_exit(&[&transient, &stale]),
            HeddleExitCode::DataErr
        );
        let refused = outcome(&[ReplicationIssueKind::NotReplicable]);
        assert_eq!(incomplete_exit(&[&refused]), HeddleExitCode::DataErr);
        let transient_after_supersede = outcome(&[
            ReplicationIssueKind::Superseded,
            ReplicationIssueKind::Transient,
        ]);
        assert_eq!(
            incomplete_exit(&[&transient_after_supersede]),
            HeddleExitCode::TempFail
        );
    }

    #[test]
    fn surface_outcome_separates_unsent_from_local_only() {
        let report = ReplicationReport {
            accepted: 2,
            issues: vec![
                issue(ReplicationIssueKind::StaleVersion),
                issue(ReplicationIssueKind::NotReplicable),
                issue(ReplicationIssueKind::Superseded),
            ],
        };
        let outcome = surface_outcome(Surface::Context, &Ok(report), TARGET);
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.count, Some(2));
        assert_eq!(outcome.unsent.len(), 1);
        assert_eq!(outcome.local_only.len(), 2);
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|error| !error.contains("superseded"))
        );

        let superseded_only = ReplicationReport {
            accepted: 1,
            issues: vec![issue(ReplicationIssueKind::Superseded)],
        };
        let outcome = surface_outcome(Surface::Context, &Ok(superseded_only), TARGET);
        assert_eq!(outcome.status, "succeeded");
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.local_only.len(), 1);

        let failed = surface_outcome(
            Surface::Discussions,
            &Err(anyhow::Error::new(wire::ProtocolError::Io(
                std::io::Error::other("reset"),
            ))),
            TARGET,
        );
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.unsent[0].record_id, None);
        assert_eq!(failed.unsent[0].kind, "transient");
    }
}
