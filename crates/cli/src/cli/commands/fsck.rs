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
    git_projection: bool,
    repair: Option<FsckRepairTarget>,
    ref_name: Option<String>,
    prefer: Option<String>,
    preview: bool,
) -> Result<()> {
    let ctx = execution_context_from_cli(cli)?;
    let repairs = match repair {
        Some(FsckRepairTarget::Git) => repair_git(ctx.require_repo()?, ref_name, prefer, preview)?,
        None => Vec::new(),
    };
    let mut report = fsck(
        &ctx,
        FsckOptions {
            full,
            thorough,
            git_projection: git_projection || repair.is_some(),
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
fn repair_git(
    repo: &repo::Repository,
    ref_name: Option<String>,
    prefer: Option<String>,
    preview: bool,
) -> Result<Vec<FsckRepair>> {
    if let Some(ref_name) = ref_name {
        return repair_git_ref(repo, &ref_name, prefer, preview);
    }
    if prefer.is_some() || preview {
        return Err(anyhow!(git_repair_ref_required_advice()));
    }
    repair_git_metadata(repo)
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

#[cfg(feature = "git-overlay")]
fn repair_git_ref(
    repo: &repo::Repository,
    ref_name: &str,
    prefer: Option<String>,
    preview: bool,
) -> Result<Vec<FsckRepair>> {
    use heddle_core::status::next_action::{
        canonical_bridge_import_ref_command, canonical_bridge_reconcile_ref_command,
        canonical_bridge_reconcile_ref_preview_command,
    };
    use ingest::ImportOptions;
    use objects::object::ThreadName;
    use refs::Head;

    use crate::bridge::git_ingest::import_git_history;

    let heddle_preview = canonical_bridge_reconcile_ref_preview_command(Some("heddle"), ref_name);
    let git_preview = canonical_bridge_reconcile_ref_preview_command(Some("git"), ref_name);

    let recovery_commands = match prefer.as_deref() {
        Some("git") => vec![canonical_bridge_import_ref_command(ref_name)],
        Some("heddle") => vec![canonical_bridge_reconcile_ref_command("heddle", ref_name)],
        None if preview => vec![heddle_preview.clone(), git_preview.clone()],
        None => return Err(anyhow!(git_repair_direction_required_advice(ref_name))),
        _ => unreachable!("clap restricts --prefer values"),
    };

    if preview {
        return Ok(recovery_commands
            .into_iter()
            .map(|command| FsckRepair {
                name: "git_projection_ref_reconcile_preview".to_string(),
                repaired: false,
                detail: command,
                count: 0,
            })
            .collect());
    }

    let prefer = prefer
        .as_deref()
        .ok_or_else(|| git_repair_direction_required_advice(ref_name))?;
    match prefer {
        "git" => {
            let mut bridge = crate::bridge::GitBridge::new(repo);
            let stats = import_git_history(
                &mut bridge,
                Some(repo.root()),
                std::slice::from_ref(&ref_name.to_string()),
                ImportOptions::default(),
                None,
            )?;
            if repo.git_overlay_current_branch()?.as_deref() == Some(ref_name) {
                repo.refs().write_head(&Head::Attached {
                    thread: ThreadName::new(ref_name),
                })?;
            }
            Ok(vec![FsckRepair {
                name: "git_projection_ref_prefer_git".to_string(),
                repaired: stats.commits_imported > 0 || stats.states_created > 0,
                detail: format!(
                    "imported {} Git commit(s) from '{ref_name}' into Heddle",
                    stats.commits_imported
                ),
                count: stats.commits_imported,
            }])
        }
        "heddle" => {
            let tn = ThreadName::new(ref_name);
            let state = repo
                .refs()
                .get_thread(&tn)?
                .ok_or_else(|| git_repair_missing_heddle_thread_advice(ref_name))?;
            repo.goto_without_record(&state)?;
            repo.refs().write_head(&Head::Attached { thread: tn })?;
            let mut bridge = crate::bridge::GitBridge::new(repo);
            match bridge.write_through_current_checkout()? {
                crate::bridge::WriteThroughOutcome::Wrote(git_oid) => Ok(vec![FsckRepair {
                    name: "git_projection_ref_prefer_heddle".to_string(),
                    repaired: true,
                    detail: format!(
                        "wrote Heddle state {} for '{ref_name}' through to Git commit {git_oid}",
                        state.short()
                    ),
                    count: 1,
                }]),
                crate::bridge::WriteThroughOutcome::Skipped(reason) => Err(anyhow!(
                    git_repair_write_through_skipped_advice(ref_name, reason.to_string(),)
                )),
            }
        }
        _ => unreachable!("clap restricts --prefer values"),
    }
}

fn git_repair_ref_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_repair_ref_required",
        "Git ref repair needs a ref name",
        "Run `heddle fsck --repair git --ref <branch> --preview` to inspect ref repair choices.",
        "--prefer or --preview was supplied without --ref",
        "repairing a Git projection ref without a ref name could mutate the wrong branch",
        "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
        "heddle fsck --repair git --ref <branch> --preview",
        vec!["heddle fsck --repair git --ref <branch> --preview".to_string()],
    )
}

fn git_repair_direction_required_advice(ref_name: &str) -> RecoveryAdvice {
    use heddle_core::status::next_action::canonical_bridge_reconcile_ref_preview_command;

    let preview_command = canonical_bridge_reconcile_ref_preview_command(None, ref_name);
    RecoveryAdvice::safety_refusal(
        "git_repair_direction_required",
        format!("Refusing to repair '{ref_name}': choose a local side before applying"),
        format!(
            "Run `{preview_command}` to inspect both local repair choices, then rerun with `--prefer heddle` or `--prefer git`."
        ),
        "no --prefer side was supplied for a non-preview Git repair",
        "applying repair without a side would need to choose whether Heddle or the local Git branch is authoritative",
        "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
        preview_command.clone(),
        vec![
            preview_command,
            canonical_bridge_reconcile_ref_preview_command(Some("heddle"), ref_name),
            canonical_bridge_reconcile_ref_preview_command(Some("git"), ref_name),
        ],
    )
}

fn git_repair_missing_heddle_thread_advice(ref_name: &str) -> RecoveryAdvice {
    use heddle_core::status::next_action::{
        canonical_adopt_ref_command, canonical_bridge_reconcile_ref_command,
    };

    let import_command = canonical_adopt_ref_command(ref_name);
    let reconcile_git_command = canonical_bridge_reconcile_ref_command("git", ref_name);
    RecoveryAdvice::safety_refusal(
        "git_repair_missing_heddle_thread",
        format!("Cannot prefer Heddle for '{ref_name}': no matching Heddle thread exists"),
        format!(
            "Import the Git ref with `{import_command}`, or repair by preferring Git with `{reconcile_git_command}`."
        ),
        format!("Heddle has no thread named '{ref_name}'"),
        "preferring Heddle would need to move Git to a Heddle state that does not exist",
        "Git refs, Heddle refs, and the worktree were left unchanged",
        import_command.clone(),
        vec![
            import_command,
            reconcile_git_command,
            "heddle thread list".to_string(),
        ],
    )
}

fn git_repair_write_through_skipped_advice(ref_name: &str, reason: String) -> RecoveryAdvice {
    use heddle_core::status::next_action::canonical_bridge_reconcile_ref_preview_command;

    let preview_command = canonical_bridge_reconcile_ref_preview_command(Some("heddle"), ref_name);
    RecoveryAdvice::safety_refusal(
        "git_repair_write_through_skipped",
        format!("Could not repair '{ref_name}' by preferring Heddle: {reason}"),
        format!("Inspect the repair plan with `{preview_command}` before retrying."),
        reason,
        "writing the Heddle state into Git could not be completed for the active checkout",
        "Heddle state was preserved; Git write-through did not report a new commit",
        preview_command.clone(),
        vec![preview_command],
    )
}

#[cfg(not(feature = "git-overlay"))]
fn repair_git(
    _repo: &repo::Repository,
    _ref_name: Option<String>,
    _prefer: Option<String>,
    _preview: bool,
) -> Result<Vec<FsckRepair>> {
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
