// SPDX-License-Identifier: Apache-2.0
//! One-command Git repository adoption.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use gix::bstr::ByteSlice;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;

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
        git_import::{import_all, import_selected_refs},
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
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

pub fn cmd_adopt(cli: &Cli, args: AdoptArgs) -> Result<()> {
    let start = resolve_path(cli, args.path.as_deref())?;
    let git_root = git_worktree_root(&start)?;
    // Shallow detect MUST happen before any Heddle sidecar bootstrap or
    // remote-note hydration. Without this, a `heddle adopt` against a
    // shallow clone burns a full `git fetch` from origin (network IO,
    // minutes on a real repo) before discovering the repo can't be
    // imported. Costs one `stat()` on the happy path.
    preflight_not_shallow(&git_root, &args.refs)?;
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
    let mut bridge = GitBridge::new(&repo);
    // The hydrate fetch is the slowest thing in adopt on a real-world
    // repo (it walks every configured remote looking for a Heddle-notes
    // ref). Tell the user before it runs so the silence makes sense.
    if checkout_has_remote(&repo) {
        progress.note(
            "checking configured Git remotes for shared Heddle notes (network)",
        );
    }
    let hydrate_span =
        tracing::info_span!("adopt.hydrate_notes", path = %repo.root().display()).entered();
    let _ = bridge.hydrate_checkout_heddle_notes_from_configured_remotes();
    drop(hydrate_span);
    progress.advance("importing commits");
    let import_span = tracing::info_span!(
        "adopt.import",
        path = %repo.root().display(),
        refs = args.refs.len()
    )
    .entered();
    let stats = if args.refs.is_empty() {
        import_all(&mut bridge, Some(repo.root()))?
    } else {
        import_selected_refs(&mut bridge, Some(repo.root()), &args.refs)?
    };
    drop(import_span);
    progress.advance("writing refs");
    progress.finish();
    let trust = build_repository_verification_state(&repo);
    let already_in_sync = stats.states_created == 0 && stats.commits_imported > 0;
    let recommended_action = action_value(&trust);
    let output = AdoptOutput {
        output_kind: "adopt",
        status: "completed",
        action: "adopt",
        adopted: true,
        initialized,
        path: repo.heddle_dir().to_path_buf(),
        refs: args.refs,
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.branches_synced,
        tags_synced: stats.tags_synced,
        skipped_non_commit_refs: stats.skipped_non_commit_refs.len(),
        partial_mirror_refs: stats.partial_mirror_refs.len(),
        already_in_sync,
        recommended_action,
        recommended_action_argv: trust.recommended_action_argv.clone(),
        recommended_action_template: trust.recommended_action_template.clone(),
        trust,
    };
    render_adopt(&output, should_output_json(cli, Some(repo.config())))
}

fn action_value(trust: &RepositoryVerificationState) -> Option<String> {
    (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone())
}

fn checkout_has_remote(repo: &Repository) -> bool {
    let Ok(git) = gix::discover(repo.root()) else {
        return false;
    };
    git.remote_names().into_iter().next().is_some()
}

fn preflight_not_shallow(git_root: &Path, refs: &[String]) -> Result<()> {
    let git = gix::discover(git_root)
        .map_err(|error| anyhow!("failed to inspect Git for shallow status: {error}"))?;
    if git.git_dir().join("shallow").is_file() {
        let retry_command = if refs.is_empty() {
            "heddle bridge git import --path <full-git-repo>".to_string()
        } else {
            "heddle bridge git import --path <full-git-repo> --ref <ref>".to_string()
        };
        bail!(
            "Shallow Git repository cannot be imported. Re-clone without --depth, or run: {retry_command}"
        );
    }
    Ok(())
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
    let git =
        gix::discover(git_root).map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?;
    let references = git
        .references()
        .map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?;
    for branch in references
        .local_branches()
        .map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?
    {
        let branch = branch.map_err(|error| anyhow!("failed to inspect Git ref: {error}"))?;
        if git_ref_points_to_commit(git_root, &branch.name().as_bstr().to_str_lossy())? {
            return Ok(true);
        }
    }
    let references = git
        .references()
        .map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?;
    for tag in references
        .tags()
        .map_err(|error| anyhow!("failed to inspect Git refs: {error}"))?
    {
        let tag = tag.map_err(|error| anyhow!("failed to inspect Git ref: {error}"))?;
        if git_ref_points_to_commit(git_root, &tag.name().as_bstr().to_str_lossy())? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn git_ref_points_to_commit(git_root: &Path, name: &str) -> Result<bool> {
    let spec = format!("{name}^{{commit}}");
    let git = gix::discover(git_root)
        .map_err(|error| anyhow!("failed to inspect Git ref '{name}': {error}"))?;
    Ok(git.rev_parse_single(spec.as_bytes().as_bstr()).is_ok())
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
    let git = gix::discover(start).map_err(|error| {
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
            &format!("{}", style::bold(&output.branches_synced.to_string()))
        )
    );
    println!(
        "  {}",
        style::field(
            "Tags ready",
            &format!("{}", style::bold(&output.tags_synced.to_string()))
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
    Ok(())
}
