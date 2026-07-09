// SPDX-License-Identifier: Apache-2.0
//! Merge command shell: parse args → bootstrap → core orchestration → render.
//!
//! Planning, apply, and operator report construction live in
//! `heddle_core::merge`. This module owns hooks, current-state bootstrap,
//! text/json rendering, recommendation scoping, and exit-code mapping.

use std::path::Path;

use anyhow::{Result, anyhow};
use heddle_core::merge::{
    MergeReport, merge_thread_into_current as core_merge_thread_into_current,
};
use repo::Repository;
use serde_json::Value;

use super::{
    action_line::print_nested_next,
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_command_json},
    operator_core::blocked_operator_exit_code,
    snapshot::ensure_current_state,
    verification_health::repository_verification_blocked_advice,
};
use crate::{
    cli::{Cli, output_is_compact, should_output_json, style},
    config::UserConfig,
};

// Re-export orchestration surface for other CLI commands and benches.
pub use heddle_core::merge::{
    ThreadPreviewReport, ThreeWayMergeOutcome, apply_merged_tree_external, bench_detect_renames,
    bench_find_merge_base, bench_three_way_merge, build_thread_preview_report,
    merge_thread_into_current, prepare_dir_for_file_replacement, try_three_way_merge_between_tips,
};

/// Historical wire name for the merge report type.
pub type MergeOutput = MergeReport;

#[allow(clippy::too_many_arguments)]
pub fn cmd_merge(
    cli: &Cli,
    track_name: String,
    message: Option<String>,
    no_commit: bool,
    preview: bool,
    with_diff: bool,
    no_semantic: bool,
    git_commit: bool,
) -> Result<()> {
    let cwd_repo = cli.open_repo()?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };

    // `pre_merge` JSON-protocol hook. Veto via non-empty `abort` aborts before
    // any tree work happens.
    let hook_manager = repo::HookManager::new(&repo);
    let hook_ctx = repo::HookContext::new(&repo);
    let pre_merge_payload = serde_json::json!({
        "source": track_name.clone(),
        "target": current_thread_name(&repo),
    });
    if let Ok(Some(resp)) = hook_manager.run_with_payload(
        repo::Hook::PreMerge,
        &hook_ctx,
        &pre_merge_payload,
        std::time::Duration::from_secs(5),
    ) && !resp.abort.is_empty()
    {
        return Err(anyhow!(RecoveryAdvice::hook_veto(
            "pre_merge",
            "merge",
            resp.abort
        )));
    }

    // Bootstrap missing current state (git-overlay first capture) before core.
    let _ = ensure_current_state(
        &repo,
        &UserConfig::load_default().unwrap_or_default(),
        Some(format!(
            "Bootstrap git-overlay before merging {}",
            track_name
        )),
    )?;

    let mut output = core_merge_thread_into_current(
        &repo,
        &track_name,
        message,
        no_commit,
        preview,
        with_diff,
        no_semantic,
        git_commit,
    )?;
    scope_merge_recommendations_to_cli_repo(cli, &mut output);

    // `post_merge` JSON-protocol hook. Best-effort; can't veto an applied merge.
    if !preview {
        let post_merge_payload = serde_json::json!({
            "state_id": output.merge_state.clone().unwrap_or_default(),
        });
        if let Err(err) = hook_manager.run_with_payload(
            repo::Hook::PostMerge,
            &hook_ctx,
            &post_merge_payload,
            std::time::Duration::from_secs(5),
        ) {
            tracing::warn!(error = %err, "post_merge hook error swallowed");
        }
    }

    if preview_did_not_run(&output) {
        return Err(anyhow!(merge_preview_blocked_advice(&output)));
    }

    let exit_code = merge_output_exit_code(&output);
    render_merge_output(cli, &repo, output)?;
    if let Some(code) = exit_code {
        std::process::exit(code);
    }
    Ok(())
}

fn merge_output_exit_code(output: &MergeOutput) -> Option<i32> {
    if output.preview_only && !preview_did_not_run(output) {
        return None;
    }
    blocked_operator_exit_code(&output.operator.status)
}

fn scope_merge_recommendations_to_cli_repo(cli: &Cli, output: &mut MergeOutput) {
    let Some(repo_path) = cli.repo.as_ref() else {
        return;
    };
    output.operator.recommended_action = output
        .operator
        .recommended_action
        .as_deref()
        .map(|action| scope_action_to_repo(action, repo_path));
    output.operator.next_action = output
        .operator
        .next_action
        .as_deref()
        .map(|action| scope_action_to_repo(action, repo_path));
}

fn scope_action_to_repo(action: &str, repo_path: &Path) -> String {
    let Some(rest) = action.strip_prefix("heddle ") else {
        return action.to_string();
    };
    if rest.starts_with("--repo ") || rest.starts_with("-R ") {
        return action.to_string();
    }
    format!(
        "heddle --repo {} {rest}",
        quote_recommended_action_arg(&repo_path.display().to_string())
    )
}

fn quote_recommended_action_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn current_thread_name(repo: &Repository) -> String {
    use refs::Head;
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread.to_string(),
        _ => String::new(),
    }
}

fn preview_did_not_run(output: &MergeOutput) -> bool {
    output.preview_only
        && output.operator.status == "blocked"
        && output.operator.message.contains("preview did not run")
}

fn merge_preview_blocked_advice(output: &MergeOutput) -> RecoveryAdvice {
    let primary_command = output
        .operator
        .recommended_action
        .as_deref()
        .or(output.operator.next_action.as_deref())
        .filter(|action| !action.trim().is_empty())
        .unwrap_or("heddle verify");
    let blockers = if output.operator.blockers.is_empty() {
        output.operator.message.clone()
    } else {
        output.operator.blockers.join("; ")
    };
    let mut advice = if let Some(trust) = output.trust.as_ref() {
        repository_verification_blocked_advice(
            "merge_preview_blocked",
            output.operator.message.clone(),
            "retrying the merge preview",
            trust,
            blockers,
            "the merge preview would otherwise describe a stale or unverifiable integration path",
            "repository state, refs, and worktree files were left unchanged",
            Some(primary_command.to_string()),
        )
    } else {
        RecoveryAdvice::safety_refusal(
            "merge_preview_blocked",
            output.operator.message.clone(),
            format!("Run `{primary_command}` before retrying the merge preview."),
            blockers,
            "the merge preview would otherwise describe a stale or unverifiable integration path",
            "repository state, refs, and worktree files were left unchanged",
            primary_command.to_string(),
            vec![primary_command.to_string()],
        )
    };
    if output.conflict_count > 0 {
        advice.extra_json_fields.insert(
            "conflict_count".to_string(),
            Value::Number(output.conflict_count.into()),
        );
        advice.extra_json_fields.insert(
            "conflicts".to_string(),
            Value::Array(
                output
                    .conflicts
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    if let Some(merge_relation) = output.merge_relation.as_ref() {
        advice.extra_json_fields.insert(
            "merge_relation".to_string(),
            Value::String(merge_relation.clone()),
        );
    }
    advice
}

impl super::compact::CompactProjection for MergeOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let mut compact = super::compact::CompactOutput::new(self.operator.action.wire_value());
        compact.status = Some(self.operator.status.clone());
        compact.blockers = self.operator.blockers.clone();
        compact.next_action = self
            .operator
            .recommended_action
            .clone()
            .or_else(|| self.operator.next_action.clone());
        compact.changed_paths = Some(self.changed_paths.clone());
        compact.changed_path_count = Some(self.changed_path_count);
        compact.conflicts = Some(self.conflicts.clone());
        compact.conflict_count = Some(self.conflict_count);
        compact
    }
}

fn render_merge_output(cli: &Cli, repo: &Repository, output: MergeOutput) -> Result<()> {
    if should_output_json(cli, None) {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["merge"], repo.capability()),
        )?;
    } else {
        if output.fast_forward
            && let Some(rest) = output.operator.message.strip_prefix("Fast-forwarded ")
        {
            println!("{} {}", style::accent("Fast-forwarded"), style::dim(rest));
        } else if output.fast_forward
            && let Some(rest) = output.operator.message.strip_prefix("Would fast-forward ")
        {
            println!("{} {}", style::warn("Would fast-forward"), style::dim(rest));
        } else {
            println!("{}", output.operator.message);
        }
        for line in &output.preview_summary {
            println!("  {}", line);
        }
        if !output.conflicts.is_empty() {
            for conflict in &output.conflicts {
                println!("  {} {}", style::error("C"), conflict);
            }
        }
        for rename in &output.renames {
            println!(
                "  {} {} → {} ({:.0}%)",
                style::accent("R"),
                rename.from,
                rename.to,
                rename.score * 100.0
            );
        }
        if let Some(diff) = &output.diff {
            println!();
            crate::cli::commands::diff::print_stat(diff);
            crate::cli::commands::diff::print_diff(diff);
        }
        if let Some(git_commit) = &output.git_commit {
            let display_len = std::cmp::min(12, git_commit.sha.len());
            println!(
                "  git commit: {}",
                style::dim(&git_commit.sha[..display_len])
            );
        }
        if let Some(next) = output
            .operator
            .recommended_action
            .as_ref()
            .or(output.operator.next_action.as_ref())
        {
            print_nested_next(next);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use heddle_core::merge::MergeAttemptPlan;
    use merge::MergeStrategy;

    /// Regression-lock for HeddleCo/heddle#503.
    #[test]
    fn merge_strategy_is_decided_once_preview_equals_apply() {
        let semantic = MergeAttemptPlan::decide(false);
        let hunk_only = MergeAttemptPlan::decide(true);
        if cfg!(feature = "semantic") {
            assert_eq!(semantic.strategy(), MergeStrategy::Semantic);
            assert!(semantic.use_semantic());
        } else {
            assert_eq!(semantic.strategy(), MergeStrategy::HunkOnly);
            assert!(!semantic.use_semantic());
        }
        assert_eq!(hunk_only.strategy(), MergeStrategy::HunkOnly);
        assert!(!hunk_only.use_semantic());
    }

    #[test]
    fn prepare_dir_for_file_replacement_removes_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("empty");
        std::fs::create_dir(&target).unwrap();
        prepare_dir_for_file_replacement(&target).expect("empty dir is removable");
        assert!(!target.exists());
    }

    #[test]
    fn prepare_dir_for_file_replacement_errors_on_non_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nonempty");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"x").unwrap();
        let err = prepare_dir_for_file_replacement(&target).expect_err("non-empty dir must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("non-empty") || msg.contains("directory"),
            "unexpected error: {msg}"
        );
        assert!(target.exists());
    }

    #[test]
    fn prepare_dir_for_file_replacement_tolerates_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("missing");
        prepare_dir_for_file_replacement(&target).expect("missing path is ok");
    }
}
