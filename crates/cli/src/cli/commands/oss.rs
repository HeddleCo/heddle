// SPDX-License-Identifier: Apache-2.0
use std::process::Command;

use anyhow::Result;
use repo::Repository;
use serde::Serialize;

#[cfg(feature = "git-overlay")]
use crate::cli::style;
use crate::cli::{should_output_json, Cli};

#[derive(Debug, Serialize)]
struct VersionOutput {
    version: &'static str,
    profile: &'static str,
    features: Vec<&'static str>,
    git_version: Option<String>,
    repository_capability: Option<String>,
    repository_root: Option<String>,
}

#[cfg(feature = "git-overlay")]
pub fn cmd_git_overlay_guide(cli: &Cli) -> Result<()> {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "topic": "git-overlay",
                "summary": "Use Heddle beside Git: start lightweight, import history when you need it, and isolate risky work in threads.",
                "steps": [
                    "heddle status",
                    "heddle bridge git import --ref <branch>",
                    "heddle start <name> --path <dir>",
                    "heddle merge <name>",
                    "heddle sync"
                ]
            })
        );
        return Ok(());
    }

    println!("{}", style::bold("Git-overlay quick start"));
    println!("Use Heddle beside Git first. Import deeper history only when a command needs it.");
    println!();
    println!("1. Inspect the repo");
    println!("   {}", style::bold("heddle status"));
    println!("2. Import the current branch when history-oriented commands ask for it");
    println!(
        "   {}",
        style::bold("heddle bridge git import --ref <branch>")
    );
    println!("3. Start isolated work without disturbing your Git checkout");
    println!(
        "   {}",
        style::bold("heddle start <topic> --path ../<topic>")
    );
    println!("4. Merge, resolve, and keep moving");
    println!("   {}", style::bold("heddle merge <topic>"));
    println!("   {}", style::bold("heddle continue"));
    println!("5. Rejoin upstream Git");
    println!("   {}", style::bold("heddle sync"));
    println!();
    println!("When unsure, run {}", style::bold("heddle doctor"));
    Ok(())
}

pub fn cmd_version(cli: &Cli, verbose: bool) -> Result<()> {
    if !verbose {
        println!("heddle {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let git_version = Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        });

    let repo = std::env::current_dir()
        .ok()
        .and_then(|cwd| Repository::open(&cwd).ok());
    let repository_capability = repo
        .as_ref()
        .map(|repo| repo.capability_label().to_string());
    let repository_root = repo.as_ref().map(|repo| repo.root().display().to_string());

    let output = VersionOutput {
        version: env!("CARGO_PKG_VERSION"),
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        features: enabled_features(),
        git_version,
        repository_capability,
        repository_root,
    };

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    println!("Heddle {}", output.version);
    println!("Build profile: {}", output.profile);
    println!("Features: {}", output.features.join(", "));
    if let Some(git_version) = &output.git_version {
        println!("Git: {git_version}");
    } else {
        println!("Git: unavailable");
    }
    if let Some(capability) = &output.repository_capability {
        println!("Repository: {capability}");
    } else {
        println!("Repository: not inside a Heddle/Git worktree");
    }
    if let Some(root) = &output.repository_root {
        println!("Root: {root}");
    }
    Ok(())
}

// Each cfg-conditional push expands to either `features.push(...)` or
// nothing depending on which features are enabled at compile time.
// Clippy's `vec_init_then_push` would have us collapse these into a
// single `vec![...]`, but that would force every variant to be either
// always-present or unconditional. Suppress the lint at this site.
#[allow(clippy::vec_init_then_push)]
fn enabled_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    #[cfg(feature = "client")]
    features.push("client");
    #[cfg(feature = "ingest")]
    features.push("ingest");
    #[cfg(feature = "local")]
    features.push("local");
    #[cfg(feature = "mount")]
    features.push("mount");
    #[cfg(feature = "observability")]
    features.push("observability");
    #[cfg(feature = "s3")]
    features.push("s3");
    #[cfg(feature = "semantic")]
    features.push("semantic");
    #[cfg(feature = "semantic-extended")]
    features.push("semantic-extended");
    #[cfg(feature = "zstd")]
    features.push("zstd");
    if features.is_empty() {
        features.push("none");
    }
    features
}