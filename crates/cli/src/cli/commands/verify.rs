// SPDX-License-Identifier: Apache-2.0
//! Repository verification proof surface.

use std::{path::Path, time::Instant};

use anyhow::{Result, anyhow};
use heddle_core::{
    MachineContractInput, RepositorySetupGuidance, RepositoryVerificationState, VerificationCheck,
    VerifyOptions, VerifyReport, repository_setup_guidance, verify as core_verify,
};
use repo::Repository;

use super::{RecoveryAdvice, action_line::print_next};
use crate::{
    cli::{Cli, should_output_json, style},
    config::UserConfig,
    perf::{ProfileField, ProfileMode, emit_profile, profile_enabled, profile_mode},
};

pub fn cmd_verify(cli: &Cli, verbose: bool) -> Result<()> {
    let body_start = Instant::now();
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd).to_path_buf();
    let prepared = verify_execution_context_from_cli(cli, &start)?;
    let output = core_verify(
        &prepared.ctx,
        VerifyOptions::new()
            .with_start_path(start)
            .with_machine_contract_input(MachineContractInput::from_coverage(
                super::verification_health::machine_contract_coverage(),
            )),
    )?;
    // Open cost is paid either in the CLI shell (injected) or inside core
    // (facade open). Exactly one side owns it; sum keeps profile truthful.
    let repo_open_ms = prepared.repo_open_ms + output.profile.repo_open_ms;
    if profile_enabled() {
        let fields = [
            ProfileField::millis("plain_git_probe_ms", output.profile.plain_git_probe_ms),
            ProfileField::millis("repo_open_ms", repo_open_ms),
            ProfileField::millis("verification_ms", output.profile.verification_ms),
            ProfileField::duration("command_body_ms", body_start.elapsed()),
        ];
        match profile_mode() {
            ProfileMode::Off => {}
            ProfileMode::Human => emit_profile("verify phases", &fields),
            ProfileMode::Jsonl => {
                emit_profile(
                    "verify plain git probe",
                    &[ProfileField::millis(
                        "plain_git_probe_ms",
                        output.profile.plain_git_probe_ms,
                    )],
                );
                emit_profile(
                    "verify repo open",
                    &[ProfileField::millis("repo_open_ms", repo_open_ms)],
                );
                emit_profile(
                    "verify repository checks",
                    &[ProfileField::millis(
                        "verification_ms",
                        output.profile.verification_ms,
                    )],
                );
                emit_profile(
                    "verify command body",
                    &[ProfileField::duration(
                        "command_body_ms",
                        body_start.elapsed(),
                    )],
                );
            }
        }
    }
    // Config comes from the single open above — never re-open for JSON mode.
    let as_json = should_output_json(cli, prepared.repo_config.as_ref());
    if !output.clean && as_json {
        return Err(anyhow!(verify_failed_advice(&output.trust)));
    }
    render_verify(&output, verbose, as_json)?;
    if !output.clean {
        return Err(anyhow!(verify_failed_advice(&output.trust)));
    }
    Ok(())
}

struct VerifyExecutionPrep {
    ctx: heddle_core::ExecutionContext,
    repo_config: Option<repo::RepoConfig>,
    /// Wall time spent opening a Heddle repository in this shell (0 when open
    /// was deferred to core, e.g. plain-Git observe).
    repo_open_ms: u128,
}

fn verify_execution_context_from_cli(cli: &Cli, start: &Path) -> Result<VerifyExecutionPrep> {
    let config = UserConfig::load_default()?;
    let verbosity = if cli.quiet {
        heddle_core::Verbosity::Quiet
    } else if cli.verbose > 0 {
        heddle_core::Verbosity::Verbose
    } else {
        heddle_core::Verbosity::Normal
    };
    let mut builder = heddle_core::ExecutionContext::builder()
        .start_path(start.to_path_buf())
        .config(config)
        .verbosity(verbosity)
        .progress(std::sync::Arc::new(heddle_core::NoopProgress))
        .warnings(std::sync::Arc::new(heddle_core::NoopWarnings));

    if let Some(op_id) = crate::operation_id::resolve_operation_id(cli)? {
        builder = builder.op_id(op_id.to_string());
    }

    // Open once when a Heddle sidecar is already present so core reuses the
    // handle and JSON mode can read config without a second open.
    //
    // Do NOT call `Repository::open` on plain Git: open auto-bootstraps a
    // sidecar for mutators, which would make verify mutate and skip the
    // plain-Git observe probe (observe-only contract).
    let open_start = Instant::now();
    let (builder, repo_config, repo_open_ms) = if heddle_sidecar_present(start) {
        match Repository::open(start) {
            Ok(repo) => {
                let repo_open_ms = open_start.elapsed().as_millis();
                let repo_config = repo.config().clone();
                (builder.repo(repo), Some(repo_config), repo_open_ms)
            }
            Err(_) => (builder, None, 0),
        }
    } else {
        (builder, None, 0)
    };

    Ok(VerifyExecutionPrep {
        ctx: builder.build(),
        repo_config,
        repo_open_ms,
    })
}

/// True when `start` or an ancestor already has a Heddle sidecar (main repo or
/// worktree pointer). Used to avoid auto-bootstrap on observe-only verify.
fn heddle_sidecar_present(start: &Path) -> bool {
    let mut current = Some(start);
    while let Some(dir) = current {
        let heddle = dir.join(".heddle");
        if heddle.is_dir()
            && (heddle.join("objects").is_dir() || heddle.join("objectstore").is_file())
        {
            return true;
        }
        current = dir.parent();
    }
    false
}

fn render_verify(output: &VerifyReport, verbose: bool, as_json: bool) -> Result<()> {
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

fn render_compact_verify(output: &VerifyReport) -> Result<()> {
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

fn render_verify_repository_context(output: &VerifyReport) {
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

fn human_verify_status(output: &VerifyReport) -> String {
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

fn compact_verify_status(output: &VerifyReport) -> String {
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

fn human_output_summary(output: &VerifyReport) -> String {
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

fn verify_status_label(output: &VerifyReport) -> &'static str {
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

fn human_clean_summary(output: &VerifyReport) -> &str {
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

fn render_compact_setup_needed(output: &VerifyReport) -> bool {
    let Some(setup) = setup_needed_guidance(output) else {
        return false;
    };
    println!();
    println!("Setup needed: {}", style::warn(&setup.setup_line));
    println!("{}", style::dim(&setup.effect));
    true
}

fn setup_needed_guidance(output: &VerifyReport) -> Option<RepositorySetupGuidance> {
    let blocker = output.trust.checks.iter().find(|check| !check.clean)?;
    if !is_import_setup_blocker(blocker) {
        return None;
    }
    repository_setup_guidance(&output.trust)
}
