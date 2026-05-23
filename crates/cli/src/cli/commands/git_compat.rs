// SPDX-License-Identifier: Apache-2.0
//! Git-muscle-memory compatibility shims.

use anyhow::{Context, Result, anyhow};
use objects::object::ChangeId;
use oplog::{OpBatch, OpRecord};
use repo::Repository;
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    checkpoint::create_git_checkpoint,
    git_overlay_health::{RepositoryTrustState, build_repository_trust_state},
    snapshot::{
        SnapshotAgentOverrides, create_snapshot, preflight_large_capture_for_compat_commit,
    },
    thread_cmd::cmd_thread,
};
use crate::{
    cli::{
        BranchArgs, Cli, CommitArgs, SwitchArgs, ThreadCommands, ThreadDropArgs, ThreadListArgs,
        ThreadRenameArgs, should_output_json, style, worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Serialize)]
struct CommitCompatOutput {
    action: &'static str,
    change_id: String,
    git_commit: String,
    summary: String,
    next: &'static str,
}

pub async fn cmd_commit_compat(cli: &Cli, args: CommitArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    preflight_large_capture_for_compat_commit(&repo, args.force)?;
    if let Some(state) = repo.current_state()? {
        let tree = repo.require_tree(&state.tree)?;
        let status = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )?;
        if status.is_clean() {
            let trust = build_repository_trust_state(&repo);
            if !trust.trusted {
                return Err(anyhow!(commit_blocked_by_trust_advice(&trust)));
            }
            return Err(anyhow!(nothing_to_commit_advice()));
        }
    }
    let user_config = UserConfig::load_default().unwrap_or_default();
    let snapshot = create_snapshot(
        &repo,
        &user_config,
        args.message.clone(),
        args.confidence,
        SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;
    let captured_state = repo
        .current_state()?
        .ok_or_else(|| anyhow!("capture succeeded but no current state was recorded"))?;
    let snapshot_batch = find_recent_snapshot_batch(&repo, &captured_state.change_id)?;
    let record = create_git_checkpoint(
        &repo,
        args.message.as_deref(),
        worktree_status_options(Some(repo.config())),
    )
    .map_err(|err| {
        anyhow!(commit_checkpoint_failed_advice(
            &snapshot.change_id,
            args.message.as_deref(),
            &err
        ))
    })?;
    let checkpoint_batch = find_recent_git_checkpoint_batch(&repo, &record.git_commit)?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;

    let output = CommitCompatOutput {
        action: "commit",
        change_id: snapshot.change_id,
        git_commit: record.git_commit,
        summary: record.summary,
        next: "heddle push",
    };

    render_commit_compat(&output, should_output_json(cli, Some(repo.config())))?;

    Ok(())
}

fn nothing_to_commit_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "nothing_to_commit",
        "nothing to commit: worktree has no changes eligible for Heddle capture",
        "Inspect the worktree with `heddle status`; make changes before running `heddle commit -m \"...\"`.",
        "the worktree has no modified, deleted, or untracked paths relative to the current Heddle state",
        "commit would not capture a new Heddle state or write a meaningful Git checkpoint",
        "repository state was left unchanged",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn commit_blocked_by_trust_advice(trust: &RepositoryTrustState) -> RecoveryAdvice {
    let primary_command = if trust.recommended_action.trim().is_empty() {
        "heddle trust".to_string()
    } else {
        trust.recommended_action.clone()
    };
    let recovery_commands = if trust.recovery_commands.is_empty() {
        vec![primary_command.clone()]
    } else {
        trust.recovery_commands.clone()
    };
    RecoveryAdvice::safety_refusal(
        "commit_blocked_by_trust",
        format!(
            "refusing to report nothing to commit: repository trust is blocked ({})",
            trust.status
        ),
        format!("Run `{primary_command}` before retrying `heddle commit`."),
        format!(
            "repository trust status is {}: {}",
            trust.status, trust.summary
        ),
        "claiming nothing to commit could hide a Git/Heddle/import/operation disagreement",
        "no capture, Git checkpoint, refs, or worktree files were changed",
        primary_command,
        recovery_commands,
    )
}

fn commit_checkpoint_failed_advice(
    change_id: &str,
    message: Option<&str>,
    err: &anyhow::Error,
) -> RecoveryAdvice {
    let recovery = checkpoint_recovery_command(message);
    RecoveryAdvice::safety_refusal(
        "commit_checkpoint_failed",
        format!("capture {change_id} was preserved, but checkpoint failed: {err}"),
        format!("Resolve the checkpoint issue, then run `{recovery}`."),
        "the Heddle capture succeeded but the Git checkpoint step failed",
        "retrying `heddle commit` could create a duplicate capture instead of checkpointing the preserved state",
        format!("captured Heddle state {change_id} was preserved"),
        recovery.clone(),
        vec![recovery],
    )
}

fn checkpoint_recovery_command(message: Option<&str>) -> String {
    format!(
        "heddle checkpoint -m {}",
        shell_double_quoted(message.unwrap_or("checkpoint"))
    )
}

fn shell_double_quoted(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' | '"' | '$' | '`' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '\n' => quoted.push_str("\\n"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn find_recent_snapshot_batch(repo: &Repository, state: &ChangeId) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::Snapshot { new_state, .. } if new_state == state
                )
            })
        })
        .ok_or_else(|| anyhow!("capture succeeded but its oplog batch was not found"))
}

fn find_recent_git_checkpoint_batch(repo: &Repository, git_commit: &str) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::GitCheckpoint { new_git_oid, .. } if new_git_oid == git_commit
                )
            })
        })
        .ok_or_else(|| anyhow!("Git checkpoint succeeded but its oplog batch was not found"))
}

fn render_commit_compat(output: &CommitCompatOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "Committed {} as Git commit {}",
            style::change_id(&output.change_id),
            style::dim(&output.git_commit[..std::cmp::min(12, output.git_commit.len())])
        );
        println!("Next: {}", output.next);
    }

    Ok(())
}

pub async fn cmd_branch_compat(cli: &Cli, args: BranchArgs) -> Result<()> {
    let delete = args.delete || args.force_delete;
    let command = match (args.name, args.new_name, delete, args.move_branch) {
        (None, None, false, false) => ThreadCommands::List(ThreadListArgs::default()),
        (Some(name), None, true, false) => ThreadCommands::Drop(ThreadDropArgs {
            thread: name,
            delete_thread: true,
            force: args.force_delete,
        }),
        (Some(old), Some(new), false, true) => {
            ThreadCommands::Rename(ThreadRenameArgs { old, new })
        }
        (Some(name), None, false, false) => ThreadCommands::Create {
            name,
            ephemeral: false,
            ttl_secs: None,
        },
        _ => {
            return Err(anyhow!(
                "unsupported branch arguments; use `heddle branch`, `heddle branch <name>`, `heddle branch -m <old> <new>`, or `heddle branch -d <name>`"
            ));
        }
    };
    cmd_thread(cli, command).await
}

pub async fn cmd_switch_compat(cli: &Cli, args: SwitchArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    if repo.refs().get_thread(&args.target)?.is_some() {
        return cmd_thread(
            cli,
            ThreadCommands::Switch {
                name: args.target,
                print_cd_path: false,
                force: args.force,
            },
        )
        .await;
    }
    super::goto::cmd_goto(cli, args.target, args.force)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_checkpoint_failure_advice_preserves_capture_and_exact_recovery() {
        let error = anyhow!("git write failed");
        let advice = commit_checkpoint_failed_advice("change-123", Some("say \"hello\""), &error);

        assert_eq!(advice.kind, "commit_checkpoint_failed");
        assert!(advice.error.contains("capture change-123 was preserved"));
        assert!(advice.error.contains("git write failed"));
        assert_eq!(
            advice.primary_command,
            "heddle checkpoint -m \"say \\\"hello\\\"\""
        );
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle checkpoint -m \"say \\\"hello\\\"\""]
        );
        assert!(advice.preserved.contains("change-123"));
    }

    #[test]
    fn nothing_to_commit_advice_names_status_recovery() {
        let advice = nothing_to_commit_advice();

        assert_eq!(advice.kind, "nothing_to_commit");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(advice.error.contains("nothing to commit"));
        assert!(advice.primary_hint().contains("heddle status"));
    }

    #[test]
    fn commit_blocked_by_trust_advice_uses_trust_recovery() {
        let trust = RepositoryTrustState {
            trusted: false,
            status: "operation_in_progress".to_string(),
            repository_mode: "git-overlay".to_string(),
            heddle_initialized: true,
            git_branch: Some("main".to_string()),
            heddle_thread: Some("main".to_string()),
            worktree_dirty: false,
            import_state: "clean".to_string(),
            mapping_state: "clean".to_string(),
            remote_drift: "clean".to_string(),
            active_operation: Some("Git merge (in-progress)".to_string()),
            default_remote: None,
            clone_verification: "not_applicable".to_string(),
            machine_contract: "available".to_string(),
            summary: "Git merge is in progress".to_string(),
            recommended_action: "heddle continue".to_string(),
            recovery_commands: vec!["heddle continue".to_string()],
            checks: Vec::new(),
        };

        let advice = commit_blocked_by_trust_advice(&trust);

        assert_eq!(advice.kind, "commit_blocked_by_trust");
        assert_eq!(advice.primary_command, "heddle continue");
        assert_eq!(advice.recovery_commands, vec!["heddle continue"]);
        assert!(advice.error.contains("repository trust is blocked"));
        assert!(advice.unsafe_condition.contains("Git merge is in progress"));
    }
}
