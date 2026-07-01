// SPDX-License-Identifier: Apache-2.0
//! Fsck command - verify repository integrity.

use anyhow::{Result, anyhow};
use heddle_core::{FsckOptions, fsck};

use super::advice::RecoveryAdvice;
use crate::cli::{Cli, execution_context_from_cli, render, should_output_json};

pub fn cmd_fsck(cli: &Cli, full: bool, thorough: bool, bridge: bool) -> Result<()> {
    let ctx = execution_context_from_cli(cli)?;
    let report = fsck(
        &ctx,
        FsckOptions {
            full,
            thorough,
            bridge,
        },
    )?;

    if should_output_json(cli, Some(ctx.require_repo()?.config())) {
        render::fsck::fsck_json(&report)?;
    } else {
        render::fsck::fsck_text(&report)?;
    }

    if !report.valid {
        return Err(anyhow!(fsck_integrity_error_advice(report.errors.len())));
    }

    Ok(())
}

fn fsck_integrity_error_advice(error_count: usize) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "repository_integrity_error",
        "Repository has integrity errors",
        "Inspect repository integrity with `heddle fsck --full`, then restore or repair the reported object/ref.",
        format!("{error_count} integrity error(s) remain after fsck"),
        "treating this repository as verified could hide missing or corrupt objects/refs",
        "no repository objects, refs, or worktree files were changed",
        "heddle fsck --full",
        vec!["heddle fsck --full".to_string()],
    )
}
