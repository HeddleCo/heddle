// SPDX-License-Identifier: Apache-2.0
//! One-command Git repository adoption.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::Repository as SleyRepository;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::ActionTemplate,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    import_progress::ImportProgress,
};
use crate::{
    bridge::{
        GitBridge,
        git_import::{import_all_with_progress, import_selected_refs_with_progress},
        git_util::ImportProgressEvent as BridgeImportProgressEvent,
    },
    cli::{AdoptArgs, Cli, should_output_json, style},
};

#[derive(Debug, Serialize)]
struct AdoptOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    adopted: bool,
    initialized: bool,
    path: PathBuf,
    refs: Vec<String>,
    commits_imported: usize,
    states_created: usize,
    branches_synced: usize,
    tags_synced: usize,
    skipped_non_commit_refs: usize,
    partial_mirror_refs: usize,
    already_in_sync: bool,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    // Adopt is a one-time bootstrap, not a per-mutation hot path, so it
    // keeps the verification block (PR B's serialize-skip applies only to
    // recurring mutations). Agents adopting a repo need to know whether
    // the post-adoption state is verified or still requires follow-up.
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Debug)]
struct AdoptImportStats {
    commits_imported: usize,
    states_created: usize,
    branches_synced: usize,
    tags_synced: usize,
    skipped_non_commit_refs: usize,
    partial_mirror_refs: usize,
}

pub fn cmd_adopt(cli: &Cli, args: AdoptArgs) -> Result<()> {
    let start = resolve_path(cli, args.path.as_deref())?;
    let git_root = git_worktree_root(&start)?;
    let initialized = !git_root.join(".heddle").exists();
    if initialized {
        preflight_importable_git_history(&git_root, &args.refs)?;
    }

    let repo = if initialized {
        Repository::bootstrap_git_overlay(&git_root)?
    } else {
        Repository::open(&git_root)?
    };
    if repo.capability() != RepositoryCapability::GitOverlay {
        bail!(
            "`heddle adopt` is for Git repositories. This checkout is already a native Heddle repository."
        );
    }

    let scope = if args.refs.is_empty() {
        "all local branches and tags".to_string()
    } else {
        format!("{} ref(s): {}", args.refs.len(), args.refs.join(", "))
    };
    let source_label = repo.root().display().to_string();
    let mut progress = ImportProgress::start(cli, &repo, &scope, &source_label);
    progress.advance("importing commits");
    let stats = import_git_history_for_adopt(&repo, &args.refs, &mut progress)?;
    progress.advance("writing refs");
    progress.finish();
    let trust = build_repository_verification_state(&repo);
    let already_in_sync = stats.states_created == 0 && stats.commits_imported > 0;
    let recommended_action = action_value(&trust);
    // The .heddle data dir lives inside the repo; render it relative to the
    // repo root so adopt output stays repo-relative and doesn't leak the
    // user's absolute home path (#551).
    let heddle_dir = repo.heddle_dir();
    let heddle_data_path = heddle_dir
        .strip_prefix(repo.root())
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| heddle_dir.to_path_buf());
    let output = AdoptOutput {
        output_kind: "adopt",
        status: "completed",
        action: "adopt",
        adopted: true,
        initialized,
        path: heddle_data_path,
        refs: args.refs,
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.branches_synced,
        tags_synced: stats.tags_synced,
        skipped_non_commit_refs: stats.skipped_non_commit_refs,
        partial_mirror_refs: stats.partial_mirror_refs,
        already_in_sync,
        recommended_action,
        recommended_action_template: trust.recommended_action_template.clone(),
        trust,
    };
    render_adopt(&output, should_output_json(cli, Some(repo.config())))
}

fn import_git_history_for_adopt(
    repo: &Repository,
    refs: &[String],
    progress: &mut ImportProgress,
) -> Result<AdoptImportStats> {
    if refs.is_empty() {
        return import_all_for_adopt(repo, progress);
    }
    import_selected_refs_for_adopt(repo, refs, progress)
}

#[cfg(feature = "ingest")]
fn import_all_for_adopt(
    repo: &Repository,
    progress: &mut ImportProgress,
) -> Result<AdoptImportStats> {
    progress.detail("checking Heddle notes");
    crate::bridge::git_core::GitBridge::hydrate_checkout_heddle_notes_without_mirror(repo.root());
    progress.detail("ordering commits");
    use ingest::{ImportOptions, import_git_into_with_options_and_progress};

    let mut on_commit = |event: ingest::ImportProgressEvent| {
        progress.commit_tick(BridgeImportProgressEvent {
            commits_imported: event.commits_imported,
            total_commits: event.total_commits,
            states_created: event.states_created,
        });
    };
    let (stats, _map) = import_git_into_with_options_and_progress(
        repo.root(),
        repo.root(),
        ImportOptions::default(),
        Some(&mut on_commit),
    )?;
    Ok(AdoptImportStats {
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.refs.threads_written,
        tags_synced: stats.refs.markers_written,
        skipped_non_commit_refs: stats.refs_seen.non_commit_skipped,
        partial_mirror_refs: 0,
    })
}

#[cfg(not(feature = "ingest"))]
fn import_all_for_adopt(
    repo: &Repository,
    progress: &mut ImportProgress,
) -> Result<AdoptImportStats> {
    import_bridge_for_adopt(repo, &[], progress)
}

fn import_selected_refs_for_adopt(
    repo: &Repository,
    refs: &[String],
    progress: &mut ImportProgress,
) -> Result<AdoptImportStats> {
    import_bridge_for_adopt(repo, refs, progress)
}

fn import_bridge_for_adopt(
    repo: &Repository,
    refs: &[String],
    progress: &mut ImportProgress,
) -> Result<AdoptImportStats> {
    let mut bridge = GitBridge::new(repo);
    let _ = bridge.hydrate_checkout_heddle_notes_from_configured_remotes();
    let mut on_commit = |event| progress.commit_tick(event);
    let stats = if refs.is_empty() {
        import_all_with_progress(&mut bridge, Some(repo.root()), Some(&mut on_commit))?
    } else {
        import_selected_refs_with_progress(
            &mut bridge,
            Some(repo.root()),
            refs,
            Some(&mut on_commit),
        )?
    };
    Ok(AdoptImportStats {
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.branches_synced,
        tags_synced: stats.tags_synced,
        skipped_non_commit_refs: stats.skipped_non_commit_refs.len(),
        partial_mirror_refs: stats.partial_mirror_refs.len(),
    })
}

fn action_value(trust: &RepositoryVerificationState) -> Option<String> {
    (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone())
}

fn preflight_importable_git_history(git_root: &Path, refs: &[String]) -> Result<()> {
    if refs.is_empty() {
        if git_repo_has_any_commit_ref(git_root)? {
            return Ok(());
        }
        return Err(anyhow!(no_git_commits_to_adopt_advice(
            git_root,
            Vec::new()
        )));
    }

    let missing = refs
        .iter()
        .filter(|name| !git_ref_points_to_commit(git_root, name).unwrap_or(false))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }

    Err(anyhow!(no_git_commits_to_adopt_advice(git_root, missing)))
}

fn git_repo_has_any_commit_ref(git_root: &Path) -> Result<bool> {
    let git = SleyRepository::discover(git_root)
        .map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?;
    for reference in git
        .references()
        .list_refs()
        .map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?
    {
        let name = reference.name.as_str();
        if (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            && git_ref_points_to_commit(git_root, name)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn git_ref_points_to_commit(git_root: &Path, name: &str) -> Result<bool> {
    let spec = format!("{name}^{{commit}}");
    let git = SleyRepository::discover(git_root)
        .map_err(|error| anyhow!("failed to inspect Git ref '{name}': {error}"))?;
    Ok(git.rev_parse(&spec).is_ok())
}

fn no_git_commits_to_adopt_advice(git_root: &Path, missing_refs: Vec<String>) -> RecoveryAdvice {
    let primary = "heddle init".to_string();
    let detail = if missing_refs.is_empty() {
        "no local branch or tag points at a Git commit".to_string()
    } else {
        format!(
            "requested ref(s) do not point at Git commits: {}",
            missing_refs.join(", ")
        )
    };
    RecoveryAdvice::safety_refusal(
        "git_history_empty",
        "No Git commits are available to adopt",
        "Run `heddle init` to start tracking this checkout before the first Git commit, or create the first Git commit and rerun `heddle adopt`.",
        format!(
            "Git repository at {} has no importable commit history; {detail}",
            git_root.display()
        ),
        "adopt would initialize Heddle metadata, but there is no Git commit to map into Heddle history",
        "Git refs, Heddle metadata, and worktree files were left unchanged",
        primary.clone(),
        vec![primary],
    )
}

fn resolve_path(cli: &Cli, positional: Option<&Path>) -> Result<PathBuf> {
    let path = match (positional, cli.repo.as_deref()) {
        (Some(positional), Some(repo_path)) => {
            if absolute_path(positional)? != absolute_path(repo_path)? {
                bail!(RecoveryAdvice::adopt_path_conflict(
                    &positional.display().to_string(),
                    &repo_path.display().to_string(),
                ));
            }
            positional.to_path_buf()
        }
        (Some(positional), None) => positional.to_path_buf(),
        (None, Some(repo_path)) => repo_path.to_path_buf(),
        (None, None) => std::env::current_dir()
            .map_err(|error| anyhow!("Failed to determine current directory: {error}"))?,
    };
    Ok(path.canonicalize().unwrap_or(path))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| anyhow!("Failed to determine current directory: {error}"))?
            .join(path))
    }
}

fn git_worktree_root(start: &Path) -> Result<PathBuf> {
    let git = SleyRepository::discover(start).map_err(|error| {
        anyhow!(RecoveryAdvice::adopt_requires_git_worktree(Some(format!(
            "Git inspection failed: {error}"
        ))))
    })?;
    let Some(workdir) = git.workdir() else {
        bail!(RecoveryAdvice::adopt_requires_git_worktree(None));
    };
    Ok(workdir.to_path_buf())
}

fn render_adopt(output: &AdoptOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(output)?);
        return Ok(());
    }

    if output.initialized {
        println!(
            "{} Heddle imported the requested Git history",
            style::ok_marker()
        );
    } else if output.already_in_sync {
        println!(
            "{} Heddle already adopted this Git repo; history is in sync",
            style::ok_marker()
        );
    } else {
        println!("{} imported Git history into Heddle", style::ok_marker());
    }
    println!(
        "  {}",
        style::field(
            "Heddle data",
            &style::dim(&output.path.display().to_string())
        )
    );
    let scope = if output.refs.is_empty() {
        "all local branches and tags".to_string()
    } else {
        output.refs.join(", ")
    };
    println!("  {}", style::field("Imported refs", &scope));
    println!(
        "  {}",
        style::field(
            "Git commits inspected",
            &style::bold(&output.commits_imported.to_string())
        )
    );
    println!(
        "  {}",
        style::field(
            "New Heddle states",
            &style::bold(&output.states_created.to_string())
        )
    );
    println!(
        "  {}",
        style::field(
            "Branches ready",
            &style::bold(&output.branches_synced.to_string()).to_string()
        )
    );
    println!(
        "  {}",
        style::field(
            "Tags ready",
            &style::bold(&output.tags_synced.to_string()).to_string()
        )
    );
    if output.skipped_non_commit_refs > 0 {
        println!(
            "{} skipped {} Git names that do not point at commits",
            style::warn_marker(),
            style::bold(&output.skipped_non_commit_refs.to_string())
        );
    }
    if output.partial_mirror_refs > 0 {
        println!(
            "{} partial Git mirror for {} names; exact SHA export degraded",
            style::warn_marker(),
            style::bold(&output.partial_mirror_refs.to_string())
        );
    }
    println!(
        "Workspace: {}",
        if output.trust.verified {
            style::accent("verified")
        } else {
            style::warn(&output.trust.status)
        }
    );
    if output.trust.worktree_state == "clean" {
        println!(
            "Git worktree: {}",
            style::accent("stays clean; import wrote .heddle metadata and imported Git history")
        );
    } else {
        println!(
            "Git worktree: {}",
            style::warn(
                "left existing changes untouched; import wrote .heddle metadata and imported Git history"
            )
        );
    }
    if !output.trust.recommended_action.is_empty() {
        print_next(&output.trust.recommended_action);
    }
    println!("New to Heddle from Git? Run `heddle help git-concepts`.");
    Ok(())
}
