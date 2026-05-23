// SPDX-License-Identifier: Apache-2.0
//! Show command.

use anyhow::{Result, anyhow};
use repo::{Repository, format_confidence};
use serde::Serialize;

use super::{
    git_overlay_health::{PlainGitTrustProbe, build_plain_git_trust_probe},
    history_target::resolve_state_id,
    snapshot::ensure_current_state,
};
use crate::{
    cli::{Cli, should_output_json, style},
    config::UserConfig,
};

#[derive(Serialize)]
struct ShowOutput {
    repository_capability: String,
    storage_model: String,
    change_id: String,
    change_id_full: String,
    content_hash: String,
    tree: String,
    parents: Vec<String>,
    intent: Option<String>,
    confidence: Option<f32>,
    principal: PrincipalInfo,
    agent: Option<AgentInfo>,
    created_at: String,
    status: String,
    verification: Option<VerificationInfo>,
    git_checkpoint: Option<String>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract.
    #[serde(skip)]
    git_overlay_import_hint: Option<ShowGitOverlayImportHintOutput>,
}

#[derive(Serialize)]
struct PrincipalInfo {
    name: String,
    email: String,
}

#[derive(Serialize)]
struct AgentInfo {
    provider: String,
    model: String,
    session_id: Option<String>,
    policy_id: Option<String>,
}

#[derive(Serialize)]
struct VerificationInfo {
    tests_passed: Option<bool>,
    tests_failed: Option<u32>,
    coverage_pct: Option<f32>,
    coverage_delta: Option<f32>,
    lint_warnings: Option<u32>,
}

#[derive(Serialize)]
struct ShowGitOverlayImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

pub fn cmd_show(cli: &Cli, state_spec: String) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    if let Some(probe) = build_plain_git_trust_probe(start)? {
        return render_plain_git_show(cli, &probe, &state_spec);
    }

    let repo = Repository::open(start)?;
    if matches!(state_spec.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
        ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before showing HEAD".to_string()),
        )?;
    }
    let id = resolve_state_id(&repo, &state_spec)?;

    let state = repo
        .store()
        .get_state(&id)?
        .ok_or_else(|| anyhow!("State not found: {}", state_spec))?;

    let output = ShowOutput {
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        git_overlay_import_hint: repo.git_overlay_import_hint()?.map(|hint| {
            ShowGitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }
        }),
        change_id: state.change_id.short(),
        change_id_full: state.change_id.to_string_full(),
        content_hash: state.compute_hash().to_hex(),
        tree: state.tree.to_hex(),
        parents: state.parents.iter().map(|p| p.short()).collect(),
        intent: state.intent.clone(),
        confidence: state.confidence,
        principal: PrincipalInfo {
            name: state.attribution.principal.name.clone(),
            email: state.attribution.principal.email.clone(),
        },
        agent: state.attribution.agent.as_ref().map(|a| AgentInfo {
            provider: a.provider.clone(),
            model: a.model.clone(),
            session_id: a.session_id.clone(),
            policy_id: a.policy_id.clone(),
        }),
        created_at: state.created_at.to_rfc3339(),
        status: format!("{:?}", state.status),
        verification: state.verification.as_ref().map(|v| VerificationInfo {
            tests_passed: v.tests_passed,
            tests_failed: v.tests_failed,
            coverage_pct: v.coverage_pct,
            coverage_delta: v.coverage_delta,
            lint_warnings: v.lint_warnings,
        }),
        git_checkpoint: repo
            .latest_git_checkpoint_for_change(&state.change_id)
            .ok()
            .flatten()
            .map(|record| record.git_commit),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_state(&output, cli.verbose > 0);
    }

    Ok(())
}

fn render_plain_git_show(cli: &Cli, probe: &PlainGitTrustProbe, state_spec: &str) -> Result<()> {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "repository_capability": "plain-git",
                "storage_model": "git",
                "requested": state_spec,
                "state": null,
                "trust": &probe.trust,
                "recommended_action": &probe.trust.recommended_action,
                "recovery_commands": &probe.trust.recovery_commands,
            }))?
        );
    } else {
        println!("Git repo, Heddle not initialized");
        if let Some(branch) = &probe.git_branch {
            println!("Git branch: {}", style::bold(branch));
        }
        println!("State: unavailable until this Git repo is initialized and imported");
        println!("Next step: {}", style::bold("heddle init"));
        if let Some(branch) = &probe.git_branch {
            println!(
                "Then: {}",
                style::bold(&format!("heddle bridge git import --ref {branch}"))
            );
        }
    }
    Ok(())
}

fn print_state(output: &ShowOutput, verbose: bool) {
    println!(
        "Repository mode: {} ({})",
        output.repository_capability, output.storage_model
    );
    if let Some(hint) = &output.git_overlay_import_hint {
        println!(
            "Git import: {} other branch(es) still live only in Git ({})",
            hint.missing_branch_count,
            crate::cli::render::preview_list(&hint.missing_branches, hint.missing_branch_count,)
        );
        println!("Next step: {}", style::dim(&hint.recommended_command));
    }
    println!();
    // Identifiers are dimmed: structurally important but not the
    // editorial focus.
    println!(
        "State: {} ({})",
        style::change_id(&output.change_id),
        style::dim(&output.content_hash[..8])
    );
    println!("Full ID: {}", style::dim(&output.change_id_full));
    println!("Tree: {}", style::dim(&output.tree));

    if !output.parents.is_empty() {
        let dimmed: Vec<String> = output.parents.iter().map(|p| style::dim(p)).collect();
        println!("Parents: {}", dimmed.join(", "));
    } else {
        println!("Parents: {}", style::dim("(root state)"));
    }

    println!();

    if let Some(intent) = &output.intent {
        // Intent line carries the human-meaningful summary; bold it.
        println!("Intent: {}", style::bold(intent));
    }

    // Render `Confidence: —` for an absent value rather than skipping
    // the line. An unset confidence is meaningful — it tells the
    // reader the agent never asserted one — and silently omitting it
    // collapses that signal into "field missing". `format_confidence`
    // is the single source of truth for the absent sentinel; see
    // `repo::snapshot_metadata` for the rationale. We band the value
    // via `style::confidence` so high/mid/low/absent are all
    // distinguishable at a glance.
    let confidence_text = format_confidence(output.confidence);
    println!(
        "Confidence: {}",
        style::confidence(output.confidence, &confidence_text)
    );

    println!();
    println!(
        "Principal: {}",
        style::principal(&output.principal.name, &output.principal.email)
    );

    if let Some(agent) = &output.agent {
        println!(
            "Agent: {}",
            style::dim(&format!("{}/{}", agent.provider, agent.model))
        );
        if let Some(session) = &agent.session_id {
            println!("  Session: {}", style::dim(session));
        }
        if let Some(policy) = &agent.policy_id {
            println!("  Policy: {}", style::dim(policy));
        }
    }

    println!();
    println!("Timestamp: {}", style::dim(&output.created_at));
    println!("Status: {}", output.status);
    if let Some(git_checkpoint) = &output.git_checkpoint {
        println!(
            "Git checkpoint: {}",
            style::dim(&git_checkpoint[..std::cmp::min(12, git_checkpoint.len())])
        );
    } else if verbose {
        // "Capture durability: local only" is the default for any
        // non-checkpointed state — informative on demand, noise on the
        // default `heddle show` view (which already implies "this state
        // doesn't carry a Git checkpoint" by the absence of the line
        // above).
        println!("Capture durability: {}", style::dim("local only"));
    }

    if let Some(v) = &output.verification {
        println!();
        println!("Verification:");
        if let Some(passed) = v.tests_passed {
            println!("  Tests passed: {}", passed);
        }
        if let Some(failed) = v.tests_failed {
            println!("  Tests failed: {}", failed);
        }
        if let Some(coverage) = v.coverage_pct {
            println!("  Coverage: {:.1}%", coverage);
        }
        if let Some(delta) = v.coverage_delta {
            println!("  Coverage delta: {:+.1}%", delta);
        }
        if let Some(warnings) = v.lint_warnings {
            println!("  Lint warnings: {}", warnings);
        }
    }
}
