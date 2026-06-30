// SPDX-License-Identifier: Apache-2.0
//! Repository verification proof surface.

use std::{path::Path, time::Instant};

use anyhow::{Result, anyhow};
use heddle_core::{
    ActionTemplate as CoreActionTemplate, MachineContractCoverage as CoreMachineContractCoverage,
    PlainGitVerifyProbe, RepositoryVerificationState, VerificationCheck, VerifyOptions,
    VerifyReport, verify as core_verify,
};
use objects::HeddleError;
use repo::Repository;

use super::{
    RecoveryAdvice,
    action_line::print_next,
    command_catalog::ActionTemplate,
    git_overlay_health::{
        self as cli_verify, build_plain_git_verification_probe, build_repository_verification_state,
    },
};
use crate::{
    cli::{Cli, should_output_json, style},
    config::UserConfig,
    perf::{ProfileField, ProfileMode, emit_profile, profile_enabled, profile_mode},
};

pub fn cmd_verify(cli: &Cli, verbose: bool) -> Result<()> {
    let body_start = Instant::now();
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd).to_path_buf();
    let ctx = verify_execution_context_from_cli(cli, &start)?;
    let output = core_verify(
        &ctx,
        VerifyOptions::new(core_plain_git_probe, core_repository_trust)
            .with_start_path(start.clone()),
    )?;
    if profile_enabled() {
        let fields = [
            ProfileField::millis("plain_git_probe_ms", output.profile.plain_git_probe_ms),
            ProfileField::millis("repo_open_ms", output.profile.repo_open_ms),
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
                    &[ProfileField::millis(
                        "repo_open_ms",
                        output.profile.repo_open_ms,
                    )],
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
    let repo_config = output
        .trust
        .heddle_initialized
        .then(|| {
            Repository::open(&start)
                .ok()
                .map(|repo| repo.config().clone())
        })
        .flatten();
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

fn verify_execution_context_from_cli(
    cli: &Cli,
    start: &Path,
) -> Result<heddle_core::ExecutionContext> {
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

    Ok(builder.build())
}

fn core_plain_git_probe(start: &Path) -> objects::error::Result<Option<PlainGitVerifyProbe>> {
    build_plain_git_verification_probe(start)
        .map(|probe| {
            probe.map(|probe| PlainGitVerifyProbe {
                trust: core_repository_verification_state(probe.trust),
            })
        })
        .map_err(|err| HeddleError::Config(err.to_string()))
}

fn core_repository_trust(repo: &Repository) -> objects::error::Result<RepositoryVerificationState> {
    Ok(core_repository_verification_state(
        build_repository_verification_state(repo),
    ))
}

pub(crate) fn core_repository_verification_state(
    state: cli_verify::RepositoryVerificationState,
) -> RepositoryVerificationState {
    RepositoryVerificationState {
        verified: state.verified,
        status: state.status,
        repository_mode: state.repository_mode,
        heddle_initialized: state.heddle_initialized,
        git_branch: state.git_branch,
        heddle_thread: state.heddle_thread,
        worktree_dirty: state.worktree_dirty,
        worktree_state: state.worktree_state,
        import_state: state.import_state,
        mapping_state: state.mapping_state,
        remote_drift: state.remote_drift,
        active_operation: state.active_operation,
        default_remote: state.default_remote,
        clone_verification: state.clone_verification,
        machine_contract: state.machine_contract,
        machine_contract_coverage: core_machine_contract_coverage(state.machine_contract_coverage),
        workflow_status: state.workflow_status,
        workflow_summary: state.workflow_summary,
        summary: state.summary,
        recommended_action: state.recommended_action,
        recommended_action_template: state.recommended_action_template.map(core_action_template),
        recovery_commands: state.recovery_commands,
        recovery_action_templates: state
            .recovery_action_templates
            .into_iter()
            .map(core_action_template)
            .collect(),
        checks: state
            .checks
            .into_iter()
            .map(core_verification_check)
            .collect(),
    }
}

pub(crate) fn core_verification_check(check: cli_verify::VerificationCheck) -> VerificationCheck {
    VerificationCheck {
        name: check.name,
        status: check.status,
        clean: check.clean,
        summary: check.summary,
        recommended_action: check.recommended_action,
        recommended_action_template: check.recommended_action_template.map(core_action_template),
        recovery_commands: check.recovery_commands,
        recovery_action_templates: check
            .recovery_action_templates
            .into_iter()
            .map(core_action_template)
            .collect(),
        details: check.details,
    }
}

pub(crate) fn core_action_template(template: ActionTemplate) -> CoreActionTemplate {
    CoreActionTemplate {
        action: template.action,
        argv_template: template.argv_template,
        required_inputs: template.required_inputs,
        agent_may_fill: template.agent_may_fill,
    }
}

pub(crate) fn core_machine_contract_coverage(
    coverage: cli_verify::MachineContractCoverage,
) -> CoreMachineContractCoverage {
    CoreMachineContractCoverage {
        status: coverage.status,
        verified_scope: coverage.verified_scope,
        advanced_scope: coverage.advanced_scope,
        summary: coverage.summary,
        catalog_commands_total: coverage.catalog_commands_total,
        catalog_mutating_commands_total: coverage.catalog_mutating_commands_total,
        json_commands_total: coverage.json_commands_total,
        json_mutating_commands_total: coverage.json_mutating_commands_total,
        json_commands_with_schema: coverage.json_commands_with_schema,
        json_commands_with_accepted_opaque_schema: coverage
            .json_commands_with_accepted_opaque_schema,
        json_commands_without_schema: coverage.json_commands_without_schema,
        verified_scope_json_commands_total: coverage.verified_scope_json_commands_total,
        verified_scope_json_commands_with_schema: coverage.verified_scope_json_commands_with_schema,
        verified_scope_json_commands_with_accepted_opaque_schema: coverage
            .verified_scope_json_commands_with_accepted_opaque_schema,
        verified_scope_json_commands_without_schema: coverage
            .verified_scope_json_commands_without_schema,
        advanced_scope_json_commands_total: coverage.advanced_scope_json_commands_total,
        advanced_scope_json_commands_with_accepted_opaque_schema: coverage
            .advanced_scope_json_commands_with_accepted_opaque_schema,
        mutating_commands_total: coverage.mutating_commands_total,
        mutating_commands_with_schema: coverage.mutating_commands_with_schema,
        mutating_commands_with_accepted_opaque_schema: coverage
            .mutating_commands_with_accepted_opaque_schema,
        mutating_commands_without_schema: coverage.mutating_commands_without_schema,
        verified_scope_mutating_commands_total: coverage.verified_scope_mutating_commands_total,
        verified_scope_mutating_commands_with_schema: coverage
            .verified_scope_mutating_commands_with_schema,
        verified_scope_mutating_commands_with_accepted_opaque_schema: coverage
            .verified_scope_mutating_commands_with_accepted_opaque_schema,
        verified_scope_mutating_commands_without_schema: coverage
            .verified_scope_mutating_commands_without_schema,
        advanced_scope_mutating_commands_total: coverage.advanced_scope_mutating_commands_total,
        advanced_scope_mutating_commands_with_accepted_opaque_schema: coverage
            .advanced_scope_mutating_commands_with_accepted_opaque_schema,
        schema_verbs_total: coverage.schema_verbs_total,
        documented_schema_verbs_total: coverage.documented_schema_verbs_total,
        undocumented_schema_verbs_total: coverage.undocumented_schema_verbs_total,
        opaque_schema_verbs_total: coverage.opaque_schema_verbs_total,
        accepted_opaque_schema_verbs_total: coverage.accepted_opaque_schema_verbs_total,
        unaccepted_opaque_schema_verbs_total: coverage.unaccepted_opaque_schema_verbs_total,
        supports_op_id_total: coverage.supports_op_id_total,
        jsonl_commands_total: coverage.jsonl_commands_total,
        missing_schema_examples: coverage.missing_schema_examples,
        missing_mutating_schema_examples: coverage.missing_mutating_schema_examples,
        verified_scope_missing_schema_examples: coverage.verified_scope_missing_schema_examples,
        verified_scope_accepted_opaque_schema_examples: coverage
            .verified_scope_accepted_opaque_schema_examples,
        advanced_scope_accepted_opaque_schema_examples: coverage
            .advanced_scope_accepted_opaque_schema_examples,
        accepted_opaque_schema_examples: coverage.accepted_opaque_schema_examples,
        unaccepted_opaque_schema_examples: coverage.unaccepted_opaque_schema_examples,
        undocumented_schema_examples: coverage.undocumented_schema_examples,
    }
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

fn setup_needed_guidance(output: &VerifyReport) -> Option<VerifySetupGuidance> {
    let blocker = output.trust.checks.iter().find(|check| !check.clean)?;
    if !is_import_setup_blocker(blocker) {
        return None;
    }
    repository_setup_guidance(&output.trust)
}

#[derive(Debug, Clone)]
struct VerifySetupGuidance {
    setup_line: String,
    effect: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepositorySetupActionKind {
    Init,
    Adopt,
    BridgeImport,
    Other,
}

fn repository_setup_guidance(trust: &RepositoryVerificationState) -> Option<VerifySetupGuidance> {
    if !matches!(trust.status.as_str(), "needs_init" | "needs_import") {
        return None;
    }
    let action = trust.recommended_action.trim();
    if action.is_empty() {
        return None;
    }
    let kind = repository_setup_action_kind(action);
    let setup_line = match kind {
        RepositorySetupActionKind::Init => {
            format!("Git repo detected; initialize Heddle with {action}")
        }
        RepositorySetupActionKind::Adopt => {
            format!("Git repo detected; connect this branch with {action}")
        }
        RepositorySetupActionKind::BridgeImport => {
            format!("Git history not imported; import it with {action}")
        }
        RepositorySetupActionKind::Other => {
            format!("Run {action} to clear the primary setup blocker")
        }
    };
    let worktree_tail = if trust.worktree_state == "clean" {
        "and the Git worktree stays clean"
    } else {
        "and existing Git worktree changes stay untouched"
    };
    let effect = match kind {
        RepositorySetupActionKind::Init => format!(
            ".heddle metadata will be created; Git commits stay in Git storage, {worktree_tail}."
        ),
        RepositorySetupActionKind::Adopt
            if trust.repository_mode == "plain-git" && !trust.heddle_initialized =>
        {
            format!(".heddle metadata will be created, Git history imported, {worktree_tail}.")
        }
        RepositorySetupActionKind::Adopt => {
            format!(".heddle metadata is present; adoption imports Git history {worktree_tail}.")
        }
        RepositorySetupActionKind::BridgeImport => {
            format!(".heddle metadata is present; Git history import runs {worktree_tail}.")
        }
        RepositorySetupActionKind::Other => {
            format!("The recommended setup command runs {worktree_tail}.")
        }
    };
    Some(VerifySetupGuidance { setup_line, effect })
}

fn repository_setup_action_kind(action: &str) -> RepositorySetupActionKind {
    if action == "heddle init" {
        RepositorySetupActionKind::Init
    } else if action.starts_with("heddle adopt") {
        RepositorySetupActionKind::Adopt
    } else if action.starts_with("heddle bridge git import") {
        RepositorySetupActionKind::BridgeImport
    } else {
        RepositorySetupActionKind::Other
    }
}
