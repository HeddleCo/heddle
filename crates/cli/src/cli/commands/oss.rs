// SPDX-License-Identifier: Apache-2.0
use std::process::Command;

use anyhow::Result;
use repo::Repository;
use serde::Serialize;

#[cfg(feature = "git-overlay")]
use crate::cli::style;
use crate::cli::{Cli, should_output_json};

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
                "summary": "Use Heddle as the daily Git-overlay loop: status, capture, checkpoint, push; use threads for isolated work.",
                "steps": [
                    "heddle status",
                    "heddle diff",
                    "heddle start <name> --path <dir>",
                    "heddle capture -m <message>",
                    "heddle checkpoint -m <message>",
                    "heddle merge <name>",
                    "heddle push"
                ]
            })
        );
        return Ok(());
    }

    println!("{}", style::bold("Git-overlay quick start"));
    println!("Use Heddle as the daily loop while keeping Git interoperability explicit.");
    println!();
    println!("1. Orient");
    println!("   {}", style::bold("heddle status"));
    println!("   {}", style::bold("heddle workspace"));
    println!("2. Inspect changes");
    println!("   {}", style::bold("heddle diff"));
    println!("3. Save work");
    println!("   {}", style::bold("heddle capture -m '<message>'"));
    println!("   {}", style::bold("heddle checkpoint -m '<message>'"));
    println!("   {}", style::dim("or: heddle commit -m '<message>'"));
    println!("4. Isolate risky work");
    println!(
        "   {}",
        style::bold("heddle start <topic> --path ../<topic>")
    );
    println!("5. Integrate");
    println!("   {}", style::bold("heddle ready"));
    println!("   {}", style::bold("heddle merge <topic>"));
    println!("   {}", style::bold("heddle continue"));
    println!("6. Sync with remotes");
    println!("   {}", style::bold("heddle pull"));
    println!("   {}", style::bold("heddle push"));
    println!();
    println!("{}", style::bold("When Heddle and Git disagree"));
    println!(
        "  Heddle clean, Git has pending changes: {}",
        style::bold("heddle checkpoint")
    );
    println!(
        "  Git changed outside Heddle: {}",
        style::bold("heddle capture -m '<message>'")
    );
    println!(
        "  History-oriented command asks for import: {}",
        style::bold("heddle bridge git import --ref <branch>")
    );
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

    let repo_path = cli.repo.clone().or_else(|| std::env::current_dir().ok());
    let repo = repo_path.and_then(|path| Repository::open(path).ok());
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
