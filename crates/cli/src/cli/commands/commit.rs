// SPDX-License-Identifier: Apache-2.0
//! Git-overlay commit command.

use anyhow::{Result, anyhow};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    checkpoint::{GitCheckpointRequest, create_git_checkpoint},
    command_catalog::ActionTemplate,
    verification_health::{
        RepositoryVerificationState, action_template, build_repository_verification_state,
        plain_git_mutation_preflight_advice,
    },
};
use crate::cli::{Cli, CommitArgs, should_output_json, style, worktree_status_options};

#[derive(Serialize)]
struct CommitOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    state_id: String,
    git_commit: String,
    summary: String,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

pub fn cmd_commit(cli: &Cli, args: CommitArgs) -> Result<()> {
    let cwd;
    let start = if let Some(path) = cli.repo.as_ref() {
        path
    } else {
        cwd = std::env::current_dir()?;
        &cwd
    };
    if let Some(advice) = plain_git_mutation_preflight_advice(start, "commit")? {
        return Err(anyhow!(advice));
    }

    let repo = cli.open_repo()?;
    if repo.source_authority() != repo::RepositorySourceAuthority::GitOverlay {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "commit_requires_git_overlay",
            "`heddle commit` writes Git-overlay source history",
            "Use `heddle capture -m \"...\"` in a native Heddle repository.",
            format!(
                "repository source authority is {}",
                repo.storage_model_label()
            ),
            "commit would try to update a Git ref where Git is not the source authority",
            "Heddle refs and worktree files were left unchanged",
            "heddle capture -m \"...\"",
            vec!["heddle capture -m \"...\"".to_string()],
        )));
    }

    let state = repo.current_state()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::safety_refusal(
            "commit_capture_required",
            "No captured Heddle state is available to commit",
            "Capture the intended work first, then commit that exact state to Git.",
            "the Git Overlay has no current captured Heddle state",
            "commit needs a captured state before it can write authoritative Git history",
            "Git refs, the Git index, Heddle refs, and worktree files were left unchanged",
            "heddle capture -m \"...\"",
            vec![
                "heddle capture -m \"...\"".to_string(),
                "heddle status".to_string(),
            ],
        ))
    })?;
    let already_current = repo
        .latest_git_checkpoint_for_state(&state.state_id)?
        .is_some();
    let record = create_git_checkpoint(
        &repo,
        GitCheckpointRequest {
            action: "commit",
            message: args.message.as_deref(),
            retry_command: "heddle commit -m \"...\"",
        },
        worktree_status_options(Some(repo.config())),
    )?;
    let trust = build_repository_verification_state(&repo);
    let recommended_action =
        (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone());
    let recommended_action_template = recommended_action.as_deref().and_then(action_template);
    let output = CommitOutput {
        output_kind: "commit",
        action: "commit",
        status: if already_current {
            "already_current"
        } else {
            "committed"
        },
        state_id: state.state_id.short(),
        git_commit: record.git_commit,
        summary: record.summary,
        recommended_action,
        recommended_action_template,
        trust,
    };

    if should_output_json(cli, Some(repo.config())) {
        crate::cli::render::write_json_stdout(&output)?;
    } else {
        let verb = if already_current {
            "Already committed"
        } else {
            "Committed"
        };
        println!(
            "{} {} as Git commit {}",
            style::ok_marker(),
            verb,
            style::state_id(&output.git_commit)
        );
        println!("  {}", style::field("Heddle state", &output.state_id));
        if let Some(next) = output.recommended_action.as_deref() {
            print_next(next);
        }
    }
    Ok(())
}
