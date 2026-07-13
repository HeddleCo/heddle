// SPDX-License-Identifier: Apache-2.0
//! Fsck command - verify repository integrity.

use anyhow::{Result, anyhow};
use heddle_core::{FsckOptions, FsckRepair, fsck};
use repo::RepositorySourceAuthority;

use super::advice::RecoveryAdvice;
use crate::cli::{Cli, execution_context_from_cli, render, should_output_json};

pub fn cmd_fsck(cli: &Cli, full: bool, thorough: bool, git_projection: bool) -> Result<()> {
    let ctx = execution_context_from_cli(cli)?;
    run_fsck(cli, &ctx, full, thorough, git_projection, None)
}

pub fn cmd_fsck_repair_git(
    cli: &Cli,
    ref_name: Option<String>,
    prefer: Option<String>,
    preview: bool,
) -> Result<()> {
    let ctx = execution_context_from_cli(cli)?;
    let repairs = repair_git(ctx.require_repo()?, ref_name, prefer, preview)?;
    run_fsck(cli, &ctx, false, false, true, Some(repairs))
}

fn run_fsck(
    cli: &Cli,
    ctx: &heddle_core::ExecutionContext,
    full: bool,
    thorough: bool,
    git_projection: bool,
    repairs: Option<Vec<FsckRepair>>,
) -> Result<()> {
    let mut report = fsck(
        ctx,
        FsckOptions {
            full,
            thorough,
            git_projection,
        },
    )?;

    if let Some(repairs) = repairs {
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
    let required_direction = match repo.source_authority() {
        RepositorySourceAuthority::GitOverlay => "git",
        RepositorySourceAuthority::Native => "heddle",
    };
    let requested_direction = prefer.as_deref().unwrap_or(required_direction);
    if requested_direction != required_direction {
        return Err(anyhow!(git_repair_authority_mismatch_advice(
            repo.source_authority(),
            requested_direction,
            ref_name.as_deref(),
        )));
    }
    if let Some(ref_name) = ref_name {
        return repair_git_ref(repo, &ref_name, required_direction, preview);
    }
    if repo.source_authority() == RepositorySourceAuthority::Native {
        return Err(anyhow!(git_repair_native_ref_required_advice()));
    }
    if preview {
        return Err(anyhow!(git_repair_ref_required_advice()));
    }
    repair_git_metadata(repo)
}

#[cfg(feature = "git-overlay")]
fn repair_git_metadata(repo: &repo::Repository) -> Result<Vec<FsckRepair>> {
    use heddle_git_projection::GitProjection;

    let mut bridge = GitProjection::new(repo);
    if !bridge.mirror_path().exists() && sley::Repository::discover(repo.root()).is_err() {
        return Ok(vec![FsckRepair {
            name: "git_projection_metadata".to_string(),
            repaired: false,
            detail: "no Git repository or legacy Bridge Mirror was found".to_string(),
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
    bridge.seed_ingest_identity_mappings_from_repo(&git_repo)?;
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
    prefer: &str,
    preview: bool,
) -> Result<Vec<FsckRepair>> {
    use heddle_git_projection::git_ingest::import_git_history;
    use ingest::ImportOptions;
    use objects::object::ThreadName;
    use refs::Head;

    if preview {
        return Ok(vec![FsckRepair {
            name: "git_projection_ref_reconcile_preview".to_string(),
            repaired: false,
            detail: git_repair_ref_command(prefer, ref_name, false),
            count: 0,
        }]);
    }

    match prefer {
        "git" => {
            let mut bridge = heddle_git_projection::GitProjection::new(repo);
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
            let mut bridge = heddle_git_projection::GitProjection::new(repo);
            match bridge.write_through_current_checkout()? {
                heddle_git_projection::WriteThroughOutcome::Wrote(git_oid) => {
                    Ok(vec![FsckRepair {
                        name: "git_projection_ref_prefer_heddle".to_string(),
                        repaired: true,
                        detail: format!(
                            "wrote Heddle state {} for '{ref_name}' through to Git commit {git_oid}",
                            state.short()
                        ),
                        count: 1,
                    }])
                }
                heddle_git_projection::WriteThroughOutcome::Skipped(reason) => Err(anyhow!(
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
        "Run `heddle fsck repair git --ref <branch> --preview` to inspect the authority-valid repair.",
        "--preview was supplied without --ref",
        "repairing a Git projection ref without a ref name could mutate the wrong branch",
        "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
        "heddle fsck repair git --ref <branch> --preview",
        vec!["heddle fsck repair git --ref <branch> --preview".to_string()],
    )
}

fn git_repair_native_ref_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_repair_native_ref_required",
        "Native Git projection repair needs a ref name",
        "Run `heddle fsck repair git --ref <branch> --preview` to inspect the Heddle-to-Git projection repair.",
        "the native repository has no projection ref target",
        "repairing every retained Git ref could overwrite unrelated adapter state",
        "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
        "heddle fsck repair git --ref <branch> --preview",
        vec!["heddle fsck repair git --ref <branch> --preview".to_string()],
    )
}

fn git_repair_authority_mismatch_advice(
    authority: RepositorySourceAuthority,
    requested: &str,
    ref_name: Option<&str>,
) -> RecoveryAdvice {
    match authority {
        RepositorySourceAuthority::GitOverlay => RecoveryAdvice::safety_refusal(
            "git_repair_requires_adoption",
            "Git owns source history in this repository",
            "Run `heddle adopt` before repairing Git from Heddle-native state.",
            format!("--prefer {requested} conflicts with git-overlay source authority"),
            "writing Heddle state through to Git would override the authoritative Git checkout",
            "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
            "heddle adopt",
            vec!["heddle adopt".to_string()],
        ),
        RepositorySourceAuthority::Native => {
            let import = ref_name.map_or_else(
                || "heddle import git".to_string(),
                heddle_core::status::next_action::canonical_git_import_ref_command,
            );
            RecoveryAdvice::safety_refusal(
                "git_repair_requires_import",
                "Heddle owns source history in this repository",
                format!(
                    "Use `{import}` to import Git data explicitly instead of treating the retained adapter as authoritative."
                ),
                format!("--prefer {requested} conflicts with native source authority"),
                "importing retained Git adapter state during repair would override Heddle-native history",
                "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
                import.clone(),
                vec![import],
            )
        }
    }
}

fn git_repair_missing_heddle_thread_advice(ref_name: &str) -> RecoveryAdvice {
    use heddle_core::status::next_action::canonical_git_import_ref_command;

    let import_command = canonical_git_import_ref_command(ref_name);
    RecoveryAdvice::safety_refusal(
        "git_repair_missing_heddle_thread",
        format!("Cannot prefer Heddle for '{ref_name}': no matching Heddle thread exists"),
        format!("Import the Git ref explicitly with `{import_command}`."),
        format!("Heddle has no thread named '{ref_name}'"),
        "preferring Heddle would need to move Git to a Heddle state that does not exist",
        "Git refs, Heddle refs, and the worktree were left unchanged",
        import_command.clone(),
        vec![import_command, "heddle thread list".to_string()],
    )
}

fn git_repair_write_through_skipped_advice(ref_name: &str, reason: String) -> RecoveryAdvice {
    let preview_command = git_repair_ref_command("heddle", ref_name, true);
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
    Err(anyhow!("fsck repair git requires the git-overlay feature"))
}

fn git_repair_ref_command(prefer: &str, ref_name: &str, preview: bool) -> String {
    let mut command = format!(
        "heddle fsck repair git --prefer {} --ref {}",
        repo::shell_quote(prefer),
        repo::shell_quote(ref_name)
    );
    if preview {
        command.push_str(" --preview");
    }
    command
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

#[cfg(all(test, feature = "git-overlay"))]
mod tests {
    use repo::Repository;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn native_rejects_git_as_repair_authority_before_mutation() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let head_before = repo.head().unwrap();

        let error = repair_git(
            &repo,
            Some("main".to_string()),
            Some("git".to_string()),
            false,
        )
        .unwrap_err();

        assert_eq!(
            error
                .downcast_ref::<RecoveryAdvice>()
                .map(|advice| advice.kind),
            Some("git_repair_requires_import")
        );
        assert_eq!(repo.head().unwrap(), head_before);
    }

    #[test]
    fn git_overlay_rejects_heddle_as_repair_authority_before_mutation() {
        let temp = TempDir::new().unwrap();
        sley::Repository::init(temp.path()).unwrap();
        let repo = Repository::init_git_overlay_sidecar(temp.path()).unwrap();
        let head_before = repo.head().unwrap();

        let error = repair_git(
            &repo,
            Some("main".to_string()),
            Some("heddle".to_string()),
            false,
        )
        .unwrap_err();

        assert_eq!(
            error
                .downcast_ref::<RecoveryAdvice>()
                .map(|advice| advice.kind),
            Some("git_repair_requires_adoption")
        );
        assert_eq!(repo.head().unwrap(), head_before);
    }

    #[test]
    fn preview_defaults_to_the_repository_authority_direction() {
        let native_dir = TempDir::new().unwrap();
        let native = Repository::init_default(native_dir.path()).unwrap();
        let native_preview = repair_git(&native, Some("main".to_string()), None, true).unwrap();
        assert_eq!(native_preview.len(), 1);
        assert!(native_preview[0].detail.contains("--prefer heddle"));

        let overlay_dir = TempDir::new().unwrap();
        sley::Repository::init(overlay_dir.path()).unwrap();
        let overlay = Repository::init_git_overlay_sidecar(overlay_dir.path()).unwrap();
        let overlay_preview = repair_git(&overlay, Some("main".to_string()), None, true).unwrap();
        assert_eq!(overlay_preview.len(), 1);
        assert!(overlay_preview[0].detail.contains("--prefer git"));
    }
}
