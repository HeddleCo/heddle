// SPDX-License-Identifier: Apache-2.0
//! Fsck command - verify repository integrity.

use anyhow::{Result, anyhow};
use heddle_core::{FsckOptions, FsckRepair, fsck};

use super::advice::RecoveryAdvice;
use crate::cli::{Cli, FsckRepairTarget, execution_context_from_cli, render, should_output_json};

pub fn cmd_fsck(
    cli: &Cli,
    full: bool,
    thorough: bool,
    bridge: bool,
    repair: Option<FsckRepairTarget>,
) -> Result<()> {
    let ctx = execution_context_from_cli(cli)?;
    let repairs = match repair {
        Some(FsckRepairTarget::Git) => repair_git_metadata(ctx.require_repo()?)?,
        None => Vec::new(),
    };
    let mut report = fsck(
        &ctx,
        FsckOptions {
            full,
            thorough,
            bridge: bridge || repair.is_some(),
        },
    )?;

    if repair.is_some() {
        report.repair_target = Some("git".to_string());
        report.repaired = repairs.iter().any(|repair| repair.repaired);
        report.repairs = repairs;
    }

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

#[cfg(feature = "git-overlay")]
fn repair_git_metadata(repo: &repo::Repository) -> Result<Vec<FsckRepair>> {
    use crate::bridge::GitBridge;

    let mut bridge = GitBridge::new(repo);
    if !bridge.mirror_path().exists() && sley::Repository::discover(repo.root()).is_err() {
        return Ok(vec![FsckRepair {
            name: "git_projection_metadata".to_string(),
            repaired: false,
            detail: "no Git repository or legacy bridge mirror was found".to_string(),
            count: 0,
        }]);
    }

    let mapping_path = bridge.mapping_path();
    let mapping_tmp_path = bridge.mapping_tmp_path();
    let tmp_existed = mapping_tmp_path.exists();
    let mapping_existed = mapping_path.exists();

    bridge.build_existing_mapping(None)?;
    let mut repairs = Vec::new();

    if tmp_existed {
        let repaired = !mapping_tmp_path.exists();
        let detail = if mapping_existed {
            "removed stale Git projection mapping temp file"
        } else {
            "promoted Git projection mapping temp file into place"
        };
        repairs.push(FsckRepair {
            name: "git_projection_mapping_tmp".to_string(),
            repaired,
            detail: detail.to_string(),
            count: usize::from(repaired),
        });
    }

    let before_seed = bridge.mapping.iter().count();
    let git_repo = bridge.open_git_repo()?;
    bridge.seed_ingest_identity_mappings_from_mirror(&git_repo)?;
    let seeded = bridge.mapping.iter().count().saturating_sub(before_seed);
    if seeded > 0 {
        bridge.save_mapping_to_disk()?;
    }
    repairs.push(FsckRepair {
        name: "git_projection_mapping_rebuild".to_string(),
        repaired: seeded > 0,
        detail: "rebuilt missing projection mappings from portable ingest identity".to_string(),
        count: seeded,
    });

    let pruned = bridge.prune_unreachable_mapping_entries()?;
    repairs.push(FsckRepair {
        name: "git_projection_mapping_prune".to_string(),
        repaired: pruned > 0,
        detail: "removed projection mappings whose Git objects are no longer reachable".to_string(),
        count: pruned,
    });

    Ok(repairs)
}

#[cfg(not(feature = "git-overlay"))]
fn repair_git_metadata(_repo: &repo::Repository) -> Result<Vec<FsckRepair>> {
    Err(anyhow!(
        "fsck --repair git requires the git-overlay feature"
    ))
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
