// SPDX-License-Identifier: Apache-2.0
//! Repository trust proof surface.

use anyhow::Result;
use repo::Repository;
use serde::Serialize;

use super::git_overlay_health::{
    RepositoryTrustState, build_plain_git_trust_probe, build_repository_trust_state,
};
use crate::cli::{Cli, should_output_json, style};

#[derive(Debug, Serialize)]
struct TrustOutput {
    trusted: bool,
    status: String,
    checks: Vec<TrustCheckOutput>,
    recommended_action: String,
    recovery_commands: Vec<String>,
    trust: RepositoryTrustState,
}

#[derive(Debug, Serialize)]
struct TrustCheckOutput {
    name: String,
    status: String,
    clean: bool,
    summary: String,
    recommended_action: Option<String>,
    recovery_commands: Vec<String>,
}

pub fn cmd_trust(cli: &Cli, verbose: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let trust = if let Some(probe) = build_plain_git_trust_probe(start)? {
        probe.trust
    } else {
        let repo = Repository::open(start)?;
        build_repository_trust_state(&repo)
    };
    let output = TrustOutput {
        trusted: trust.trusted,
        status: trust.status.clone(),
        checks: trust
            .checks
            .iter()
            .map(|check| TrustCheckOutput {
                name: check.name.clone(),
                status: check.status.clone(),
                clean: check.clean,
                summary: check.summary.clone(),
                recommended_action: check.recommended_action.clone(),
                recovery_commands: check.recovery_commands.clone(),
            })
            .collect(),
        recommended_action: trust.recommended_action.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        trust,
    };
    render_trust(cli, &output, verbose)
}

fn render_trust(cli: &Cli, output: &TrustOutput, verbose: bool) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
        return Ok(());
    }

    println!("{}", style::bold("Heddle trust"));
    println!(
        "Status: {}",
        if output.trusted {
            style::accent(&output.status)
        } else {
            style::warn(&output.status)
        }
    );
    println!();
    for row in [
        "Git",
        "Heddle",
        "Mapping",
        "Worktree",
        "Remote",
        "Operation",
        "Machine contract",
        "Clone",
    ] {
        let check = output
            .checks
            .iter()
            .find(|check| check.name.eq_ignore_ascii_case(row));
        match check {
            Some(check) => println!(
                "{:<18} {} {}",
                row,
                if check.clean {
                    style::accent("ok")
                } else {
                    style::warn(&check.status)
                },
                style::dim(&check.summary)
            ),
            None => println!("{:<18} {}", row, style::dim("not checked")),
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
    if !output.recommended_action.is_empty() {
        println!();
        println!("Next: {}", style::bold(&output.recommended_action));
    }
    if !output.recovery_commands.is_empty() && verbose {
        for command in &output.recovery_commands {
            println!("Recovery: {}", style::bold(command));
        }
    }
    Ok(())
}
