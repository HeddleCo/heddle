// SPDX-License-Identifier: Apache-2.0
//! Repository verification proof surface.

use anyhow::{Result, anyhow};
use repo::Repository;
use serde::Serialize;

use super::{
    RecoveryAdvice,
    action_line::print_next,
    git_overlay_health::{
        RepositoryVerificationState, VerificationCheck, build_plain_git_verification_probe,
        build_repository_verification_state, repository_setup_guidance,
    },
};
use crate::cli::{Cli, should_output_json, style};

#[derive(Debug, Serialize)]
struct VerifyOutput {
    output_kind: &'static str,
    clean: bool,
    repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_context: Option<crate::cli::render::RepositoryContextInfo>,
    #[serde(flatten)]
    trust: RepositoryVerificationState,
}

pub fn cmd_verify(cli: &Cli, verbose: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let (trust, presentation, repo_config) =
        if let Some(probe) = build_plain_git_verification_probe(start)? {
            (
                probe.trust,
                crate::cli::render::RepositoryPresentation {
                    label: crate::cli::render::repository_mode_label("plain-git", "git-only"),
                    context: None,
                },
                None,
            )
        } else {
            let repo = Repository::open(start)?;
            let trust = build_repository_verification_state(&repo);
            let presentation = crate::cli::render::repository_presentation(&repo, None, None);
            let config = repo.config().clone();
            (trust, presentation, Some(config))
        };
    let output = VerifyOutput {
        output_kind: "verify",
        clean: trust.verified,
        repository_label: presentation.label,
        repository_context: presentation.context,
        trust,
    };
    let as_json = should_output_json(cli, repo_config.as_ref());
    if !output.clean && as_json {
        return Err(anyhow!(verify_failed_advice(&output.trust)));
    }
    render_verify(&output, verbose, as_json)?;
    if !output.clean {
        return Err(anyhow!(verify_failed_advice(&output.trust)));
    }
    Ok(())
}

fn render_verify(output: &VerifyOutput, verbose: bool, as_json: bool) -> Result<()> {
    if as_json {
        crate::cli::render::write_json_stdout(output)?;
        return Ok(());
    }
    if !verbose {
        return render_compact_verify(output);
    }

    println!("{}", style::bold("Heddle verify"));
    println!("Repository: {}", output.repository_label);
    render_verify_repository_context(output);
    render_verify_observe_only_note();
    let trust_label = verify_status_label(output);
    let status = human_verify_status(output);
    println!(
        "{trust_label}: {}",
        if output.trust.verified {
            style::accent(&status)
        } else {
            style::warn(&status)
        }
    );
    println!();
    for (row, label) in [
        ("Git", "Git"),
        ("Heddle", "Heddle"),
        ("Mapping", "Mapping"),
        ("Worktree", "Worktree"),
        ("Remote", "Remote"),
        ("Operation", "Operation"),
        ("Workflow", "Workflow"),
        ("Machine contract", "Machine contract"),
        ("Clone", "Checkout"),
    ] {
        let check = output
            .trust
            .checks
            .iter()
            .find(|check| check.name.eq_ignore_ascii_case(row));
        let summary = check.map(|check| check.summary.as_str());
        match check {
            Some(check) => {
                let (status, summary) = (
                    if check.status == "not_applicable" {
                        style::dim("n/a")
                    } else if check.clean && check.status != "clean" {
                        style::accent(&check.status)
                    } else if check.clean {
                        style::accent("ok")
                    } else {
                        style::warn(&human_check_status(check))
                    },
                    human_summary(check, summary, verbose),
                );
                println!("{:<18} {} {}", label, status, style::dim(&summary));
            }
            None => println!("{:<18} {}", label, style::dim("not checked")),
        }
        if verbose
            && let Some(check) = check
            && !check.details.is_empty()
        {
            let details = check
                .details
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("{:<18} {}", "", style::dim(&details));
        }
    }
    if verbose {
        println!();
        println!("Repository mode: {}", output.trust.repository_mode);
        if let Some(branch) = &output.trust.git_branch {
            println!("Git branch: {branch}");
        }
        if let Some(thread) = &output.trust.heddle_thread {
            println!("Heddle thread: {thread}");
        }
    }
    if !output.trust.recommended_action.is_empty() {
        println!();
        print_next(&output.trust.recommended_action);
    }
    if !output.trust.recovery_commands.is_empty() && verbose {
        for command in &output.trust.recovery_commands {
            println!("Recovery: {}", style::bold(command));
        }
    }
    Ok(())
}

fn render_compact_verify(output: &VerifyOutput) -> Result<()> {
    println!("{}", style::bold("Heddle verify"));
    println!("Repository: {}", output.repository_label);
    render_verify_repository_context(output);
    render_verify_observe_only_note();
    let trust_label = "Workspace";
    let status = compact_verify_status(output);
    println!(
        "{trust_label}: {}",
        if output.trust.verified {
            style::accent(&status)
        } else {
            style::warn(&status)
        }
    );
    let summary = human_output_summary(output);
    let blocker = output.trust.checks.iter().find(|check| !check.clean);
    let blocker_summary = blocker.map(|check| human_summary(check, None, false));
    if !summary.is_empty() && blocker_summary.as_deref() != Some(summary.as_str()) {
        println!("{}", style::dim(&summary));
    }
    if render_compact_setup_needed(output) {
        // Setup wording is deliberately plain here; verbose/JSON still expose
        // the individual Heddle and mapping checks for diagnostic callers.
    } else if let Some(blocker) = blocker {
        println!();
        if is_worktree_save_blocker(blocker) {
            println!(
                "Changes to save: {}",
                style::dim(&human_summary(blocker, None, false))
            );
        } else if is_import_setup_blocker(blocker) {
            println!(
                "Setup needed: {}",
                style::dim(&human_summary(blocker, None, false))
            );
        } else if is_checkpoint_blocker(blocker) {
            println!(
                "Saved in Heddle: {}",
                style::dim(&human_summary(blocker, None, false))
            );
        } else {
            println!(
                "Blocked: {}",
                style::dim(&human_summary(blocker, None, false))
            );
        }
    } else if summary.is_empty() {
        println!("{}", style::dim("All checks agree."));
    }
    if !output.trust.recommended_action.is_empty() {
        println!();
        print_next(&output.trust.recommended_action);
    }
    println!();
    println!("Proof: {}", style::bold("heddle verify --verbose"));
    Ok(())
}

fn render_verify_observe_only_note() {
    println!(
        "Mode: {}",
        style::dim("observe-only; no refs, objects, index, or worktree files are changed")
    );
}

fn render_verify_repository_context(output: &VerifyOutput) {
    let Some(context) = &output.repository_context else {
        return;
    };
    if let Some(parent_repository) = &context.parent_repository {
        println!("Parent repo: {}", parent_repository);
    }
    if let Some(target_thread) = &context.target_thread {
        println!("Target thread: {}", target_thread);
    }
    if let Some(parent_thread) = &context.parent_thread {
        println!("Parent thread: {}", parent_thread);
    }
}

fn human_verify_status(output: &VerifyOutput) -> String {
    if let Some(check) = output.trust.checks.iter().find(|check| !check.clean) {
        if is_worktree_save_blocker(check) {
            return "changes to save".to_string();
        }
        if is_import_setup_blocker(check) {
            return "setup needed".to_string();
        }
        if is_checkpoint_blocker(check) {
            return "saved in Heddle; checkpoint needed".to_string();
        }
    }
    output.trust.status.clone()
}

fn compact_verify_status(output: &VerifyOutput) -> String {
    if output.trust.verified {
        "verified".to_string()
    } else {
        human_verify_status(output)
    }
}

fn human_check_status(check: &VerificationCheck) -> String {
    if is_worktree_save_blocker(check) {
        "changes".to_string()
    } else if is_import_setup_blocker(check) {
        "setup needed".to_string()
    } else if is_checkpoint_blocker(check) {
        "checkpoint needed".to_string()
    } else {
        check.status.clone()
    }
}

fn human_output_summary(output: &VerifyOutput) -> String {
    if let Some(check) = output.trust.checks.iter().find(|check| !check.clean) {
        if setup_needed_guidance(output).is_some() {
            return String::new();
        }
        if is_worktree_save_blocker(check)
            || is_import_setup_blocker(check)
            || is_checkpoint_blocker(check)
        {
            return human_summary(check, None, false);
        }
    }
    human_clean_summary(output).to_string()
}

fn is_worktree_save_blocker(check: &VerificationCheck) -> bool {
    check.name == "Worktree" && matches!(check.status.as_str(), "dirty_worktree" | "uncaptured")
}

fn is_import_setup_blocker(check: &VerificationCheck) -> bool {
    matches!(check.name.as_str(), "Mapping" | "Heddle")
        && matches!(check.status.as_str(), "needs_import" | "needs_init")
}

fn is_checkpoint_blocker(check: &VerificationCheck) -> bool {
    check.name == "Worktree" && check.status == "needs_checkpoint"
}

fn verify_status_label(output: &VerifyOutput) -> &'static str {
    if output.trust.repository_mode == "git-overlay" || output.trust.repository_mode == "plain-git"
    {
        "Git and Heddle"
    } else {
        "Repository verification"
    }
}

fn verify_failed_advice(verification: &RepositoryVerificationState) -> RecoveryAdvice {
    let primary_command = if verification.recommended_action.trim().is_empty() {
        "heddle status".to_string()
    } else {
        verification.recommended_action.clone()
    };
    let mut recovery_commands = verification.recovery_commands.clone();
    if recovery_commands.is_empty() || recovery_commands[0] != primary_command {
        recovery_commands.insert(0, primary_command.clone());
    }
    let mut advice = RecoveryAdvice::safety_refusal(
        "verify_failed",
        format!("Repository is not verified: {}", verification.status),
        format!("Run `{primary_command}` to clear the primary verification blocker."),
        verification.summary.clone(),
        "`heddle verify` is a strict proof gate and returns nonzero until every verification check is clean",
        "verify is observe-only; repository objects, refs, index, and worktree files were left unchanged",
        primary_command,
        recovery_commands,
    );
    if let Ok(value) = serde_json::to_value(verification) {
        advice
            .extra_json_fields
            .insert("verification".to_string(), value);
    }
    advice
}

fn human_clean_summary(output: &VerifyOutput) -> &str {
    if output.trust.summary == "Git overlay and Heddle agree" {
        if output.trust.recommended_action.is_empty() {
            "Nothing to do. Workspace verified."
        } else if output.trust.recommended_action.contains("push") {
            "Local work is ready to publish."
        } else if output.trust.recommended_action.contains("land") {
            "Thread is ready to land."
        } else {
            "Workspace verified."
        }
    } else {
        &output.trust.summary
    }
}

fn human_summary(
    check: &VerificationCheck,
    override_summary: Option<&str>,
    _verbose: bool,
) -> String {
    if is_worktree_save_blocker(check) {
        let count = check
            .details
            .get("dirty_path_count")
            .and_then(|count| count.parse::<usize>().ok());
        return match count {
            Some(1) => "1 path has unsaved changes".to_string(),
            Some(count) => format!("{count} paths have unsaved changes"),
            None => "worktree has unsaved changes".to_string(),
        };
    }
    if is_import_setup_blocker(check) {
        return override_summary
            .unwrap_or("import this branch tip before comparing Heddle state")
            .replace("still need Heddle import", "need Heddle setup");
    }
    if is_checkpoint_blocker(check) {
        return override_summary
            .unwrap_or("saved in Heddle; run checkpoint or commit to write Git")
            .replace(
                "captured in Heddle but not checkpointed to Git",
                "saved in Heddle and ready to checkpoint to Git",
            );
    }
    override_summary.unwrap_or(&check.summary).to_string()
}

fn render_compact_setup_needed(output: &VerifyOutput) -> bool {
    let Some(setup) = setup_needed_guidance(output) else {
        return false;
    };
    println!();
    println!("Setup needed: {}", style::warn(&setup.setup_line));
    println!("{}", style::dim(&setup.effect));
    true
}

fn setup_needed_guidance(
    output: &VerifyOutput,
) -> Option<super::git_overlay_health::RepositorySetupGuidance> {
    let blocker = output.trust.checks.iter().find(|check| !check.clean)?;
    if !is_import_setup_blocker(blocker) {
        return None;
    }
    repository_setup_guidance(&output.trust)
}
