// SPDX-License-Identifier: Apache-2.0
#[cfg(feature = "git-overlay")]
use anyhow::Result;
#[cfg(feature = "git-overlay")]
use heddle_core::oss_plan::git_overlay_guide_json;

#[cfg(feature = "git-overlay")]
use crate::cli::style;
#[cfg(feature = "git-overlay")]
use crate::cli::{Cli, should_output_json};

#[cfg(feature = "git-overlay")]
pub fn cmd_git_overlay_guide(cli: &Cli) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", git_overlay_guide_json());
        return Ok(());
    }

    println!("{}", style::bold("Git-overlay workflow"));
    println!("Use Heddle as the daily loop with Git interoperability kept explicit.");
    println!();
    println!("1. Orient");
    println!("   {}", style::bold("heddle status"));
    println!("   {}", style::bold("heddle init"));
    println!(
        "   {}",
        style::dim("create the Heddle sidecar; Git commits stay in .git")
    );
    println!("2. Inspect changes");
    println!("   {}", style::bold("heddle diff"));
    println!("3. Save work");
    println!("   {}", style::bold("heddle commit -m '<message>'"));
    println!(
        "   {}",
        style::dim(
            "advanced split: heddle capture -m '<message>' && heddle checkpoint -m '<message>'"
        )
    );
    println!("4. Isolate risky work");
    println!("   {}", style::bold("heddle start <name> --path ../<name>"));
    println!("5. Integrate");
    println!("   {}", style::bold("heddle ready"));
    println!(
        "   {}",
        style::bold("heddle land --thread <name> --no-push")
    );
    println!("6. Sync with remotes");
    println!("   {}", style::bold("heddle pull"));
    println!("   {}", style::bold("heddle push"));
    println!("7. Recover or prove state");
    println!("   {}", style::bold("heddle undo"));
    println!("   {}", style::bold("heddle verify"));
    println!();
    println!("{}", style::bold("State-specific recovery"));
    println!(
        "  Worktree has unsaved edits: {}",
        style::bold("heddle commit -m '<message>'")
    );
    println!(
        "  Captured in Heddle but not Git: {}",
        style::bold("heddle checkpoint -m '<message>'")
    );
    println!(
        "  Convert Git history to native Heddle storage: {}",
        style::bold("heddle adopt --ref <branch>")
    );
    println!();
    println!("When unsure, run {}", style::bold("heddle verify"));
    Ok(())
}
