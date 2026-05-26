// SPDX-License-Identifier: Apache-2.0
//! Bridge command implementations.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::{
    action_line::{print_next, print_next_step, print_optional},
    advice::RecoveryAdvice,
    git_overlay_health::{
        GitOverlayHealth, GitOverlayHealthCheck, RepositoryVerificationState, action_argv,
        action_template, build_git_overlay_health, build_plain_git_verification_probe,
        build_repository_verification_state, canonical_adopt_ref_command,
        canonical_bridge_import_ref_command, canonical_bridge_reconcile_ref_command,
        canonical_bridge_reconcile_ref_preview_command, serialize_empty_action_as_null,
    },
    import_progress::ImportProgress,
    remote::resolve_default_remote_name,
};
use crate::{
    bridge::{
        GitBridge,
        git_core::clone_url_to_bare,
        git_export::export_all,
        git_import::{import_all, import_selected_refs},
    },
    cli::{Cli, GitCommands, cli_args::GitSource, should_output_json, style},
};

/// A `GitSource` resolved to an on-disk path. For URL sources we own a
/// scratch directory whose `Drop` cleans up the cloned bare repo after
/// import finishes; for path sources there's nothing to clean up.
struct ResolvedSource {
    path: PathBuf,
    _temp: Option<ScratchDir>,
}

impl ResolvedSource {
    fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Serialize)]
struct BridgeGitPushOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    success: bool,
    pushed: bool,
    changed: bool,
    transport: &'static str,
    remote: String,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct BridgeGitPullOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    success: bool,
    pulled: bool,
    changed: bool,
    transport: &'static str,
    remote: String,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

fn bridge_git_push_output(
    remote: String,
    trust: RepositoryVerificationState,
) -> BridgeGitPushOutput {
    BridgeGitPushOutput {
        output_kind: "bridge_git_push",
        action: "bridge git push",
        status: "pushed",
        success: true,
        pushed: true,
        changed: true,
        transport: "git",
        remote,
        trust,
    }
}

fn bridge_git_pull_output(
    remote: String,
    changed: bool,
    trust: RepositoryVerificationState,
) -> BridgeGitPullOutput {
    BridgeGitPullOutput {
        output_kind: "bridge_git_pull",
        action: "bridge git pull",
        status: if changed { "updated" } else { "up_to_date" },
        success: true,
        pulled: changed,
        changed,
        transport: "git",
        remote,
        trust,
    }
}

/// Owned scratch directory that removes itself on drop. Hand-rolled rather
/// than pulling `tempfile` into the cli's runtime deps just for this.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new_in(parent: &Path, prefix: &str) -> Result<Self> {
        std::fs::create_dir_all(parent)?;
        // Per-process counter + nanos is enough uniqueness for our scratch
        // dirs; we never share a parent between processes for this.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let suffix = format!("{nanos:x}-{}", std::process::id());
        let path = parent.join(format!("{prefix}{suffix}"));
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best-effort cleanup; if it fails we leave the dir behind for the
        // user to inspect rather than masking a real problem.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Resolve a `GitSource` into an on-disk path, cloning URL sources into a
/// scratch directory under `<heddle_root>/.heddle/tmp/import-<rand>`. The
/// returned `ResolvedSource` keeps the scratch dir alive for the duration
/// of the import; once it goes out of scope the cloned repo is removed.
fn resolve_source(repo: &Repository, source: GitSource) -> Result<ResolvedSource> {
    match source {
        GitSource::Path(p) => Ok(ResolvedSource {
            path: p,
            _temp: None,
        }),
        GitSource::Url(url) => {
            // Stage clones inside the heddle dir (rather than $TMPDIR) so a
            // crash mid-clone leaves obvious cleanup pointers next to the
            // repo it was for, not buried under the OS temp root.
            let tmp_root = repo.heddle_dir().join("tmp");
            let scratch = ScratchDir::new_in(&tmp_root, "import-")?;
            clone_url_to_bare(&url, &scratch.path, None, None)?;
            Ok(ResolvedSource {
                path: scratch.path.clone(),
                _temp: Some(scratch),
            })
        }
    }
}

/// Wire shape for `heddle bridge git status --output json`. This is the
/// canonical surface for import-hint information; other `--output json`
/// outputs no longer include it. Optional fields are emitted as
/// explicit `null` rather than omitted, matching the discipline used
/// across the CLI's JSON outputs.
#[derive(Serialize)]
struct BridgeGitStatusOutput {
    output_kind: &'static str,
    repository_capability: String,
    storage_model: String,
    /// Path on disk to the bridge mirror, when initialized.
    mirror_path: Option<String>,
    /// `true` when `.heddle/git` has been seeded with a mirror.
    mirror_initialized: bool,
    /// `Some(...)` when one or more local Git branches exist that
    /// haven't been imported yet. `None` when the bridge is in sync.
    git_overlay_import_hint: Option<BridgeGitImportHintOutput>,
    git_overlay_health: GitOverlayHealth,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_command_argv: Vec<Vec<String>>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct BridgeGitImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

#[derive(Serialize)]
struct BridgeGitImportOutput {
    output_kind: &'static str,
    status: String,
    action: &'static str,
    summary: String,
    commits_imported: usize,
    states_created: usize,
    branches_synced: usize,
    tags_synced: usize,
    skipped_non_commit_refs: usize,
    partial_mirror_refs: usize,
    already_in_sync: bool,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_command_argv: Vec<Vec<String>>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct BridgeGitReconcileOutput {
    output_kind: &'static str,
    status: String,
    action: &'static str,
    prefer: Option<String>,
    ref_name: String,
    preview: bool,
    summary: String,
    recommended_action: Option<String>,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_command_argv: Vec<Vec<String>>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct BridgeGitSyncOutput {
    output_kind: &'static str,
    status: String,
    action: &'static str,
    summary: String,
    states_exported: usize,
    commits_imported: usize,
    threads_synced: usize,
    markers_synced: usize,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_command_argv: Vec<Vec<String>>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

fn cmd_bridge_git_status(cli: &Cli, repo: &Repository) -> Result<()> {
    let bridge = GitBridge::new(repo);
    let mirror_path = bridge.mirror_path().to_path_buf();
    let mirror_initialized = mirror_path.exists();
    let import_hint = repo.git_overlay_import_hint().unwrap_or(None);
    let git_overlay_health = build_git_overlay_health(repo);
    let trust = RepositoryVerificationState::from_health(repo, git_overlay_health.clone());
    let output = BridgeGitStatusOutput {
        output_kind: "bridge_git_status",
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        mirror_path: Some(mirror_path.display().to_string()),
        mirror_initialized,
        git_overlay_import_hint: import_hint.map(|hint| BridgeGitImportHintOutput {
            current_branch: hint.current_branch,
            missing_branch_count: hint.missing_branch_count,
            missing_branches: hint.missing_branches,
            recommended_command: hint.recommended_command,
        }),
        git_overlay_health,
        recommended_action: trust.recommended_action.clone(),
        recommended_action_argv: trust.recommended_action_argv.clone(),
        recommended_action_template: trust.recommended_action_template.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        recovery_command_argv: trust.recovery_command_argv.clone(),
        trust,
    };
    render_bridge_git_status(&output, should_output_json(cli, Some(repo.config())));
    Ok(())
}

fn render_bridge_git_status(output: &BridgeGitStatusOutput, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(output).expect("bridge git status JSON serializes")
        );
        return;
    }
    println!(
        "Repository: {}",
        crate::cli::render::repository_mode_label(
            &output.repository_capability,
            &output.storage_model
        )
    );
    if output.mirror_initialized {
        println!(
            "Mirror: {} (initialized)",
            style::dim(output.mirror_path.as_deref().unwrap_or(""))
        );
    } else {
        let mirror_note = if output.trust.status == "needs_import" {
            "not initialized yet; the import step will create it".to_string()
        } else if output.trust.status == "needs_init" {
            format!(
                "not initialized yet; run `{}`",
                output.recommended_action.as_str()
            )
        } else {
            "not initialized yet".to_string()
        };
        println!(
            "Mirror: {} ({mirror_note})",
            style::dim(output.mirror_path.as_deref().unwrap_or(""))
        );
    }
    match &output.git_overlay_import_hint {
        Some(hint) => {
            let current_branch_needs_import = hint
                .missing_branches
                .iter()
                .any(|branch| branch == &hint.current_branch);
            if current_branch_needs_import {
                println!(
                    "{}",
                    git_import_required_summary(&hint.missing_branches, hint.missing_branch_count,)
                );
                print_next_step(&hint.recommended_command);
            } else {
                println!(
                    "{}",
                    crate::cli::render::git_only_branch_summary(
                        &hint.missing_branches,
                        hint.missing_branch_count,
                    )
                );
                print_optional(&hint.recommended_command);
            }
        }
        None => println!(
            "Git import: {}",
            if output
                .trust
                .checks
                .iter()
                .any(|check| check.name == "Mapping" && check.status == "no_commits")
            {
                "no commits to import yet"
            } else {
                "in sync"
            }
        ),
    }
    println!(
        "Git overlay health: {}",
        if output.trust.verified {
            style::accent(&output.trust.summary)
        } else {
            style::warn(&output.trust.summary)
        }
    );
    if let Some(command) = output.trust.recovery_commands.first() {
        println!("Recovery: {}", style::bold(command));
    }
}

fn render_bridge_git_import(
    cli: &Cli,
    repo: &Repository,
    output: &BridgeGitImportOutput,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(output)?);
        return Ok(());
    }

    println!("{}", output.summary);
    println!(
        "  {}",
        style::field(
            "commits",
            &style::bold(&output.commits_imported.to_string())
        )
    );
    println!(
        "  {}",
        style::field(
            "states created",
            &style::bold(&output.states_created.to_string())
        )
    );
    println!(
        "  {}",
        style::field(
            "branches",
            &format!(
                "{} synced to threads",
                style::bold(&output.branches_synced.to_string())
            )
        )
    );
    println!(
        "  {}",
        style::field(
            "tags",
            &format!(
                "{} synced to markers",
                style::bold(&output.tags_synced.to_string())
            )
        )
    );
    if output.skipped_non_commit_refs > 0 {
        println!(
            "{} skipped {} non-commit-pointing refs",
            style::warn_marker(),
            style::bold(&output.skipped_non_commit_refs.to_string())
        );
    }
    if output.partial_mirror_refs > 0 {
        println!(
            "{} partial mirror for {} refs; SHA-stable export degraded",
            style::warn_marker(),
            style::bold(&output.partial_mirror_refs.to_string())
        );
    }
    println!();
    println!("{}", style::section("Verification"));
    println!(
        "  {}",
        style::field("status", &style::thread_state(&output.trust.status))
    );
    println!("  {}", output.trust.summary);
    if !output.recommended_action.is_empty() {
        println!();
        print_next(&output.recommended_action);
    }
    Ok(())
}

fn render_bridge_git_reconcile(
    cli: &Cli,
    repo: &Repository,
    output: &BridgeGitReconcileOutput,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", output.summary);
        println!(
            "Verification: {}",
            if output.trust.verified {
                style::accent(&output.trust.summary)
            } else {
                style::warn(&output.trust.summary)
            }
        );
        for command in &output.recovery_commands {
            if output.preview && output.prefer.is_none() {
                println!("Option: {}", style::bold(command));
            } else if output.preview {
                println!("To apply: {}", style::bold(command));
            } else {
                println!("Recovery: {}", style::bold(command));
            }
        }
    }
    Ok(())
}

fn git_import_required_summary(branches: &[String], total: usize) -> String {
    let noun = if total == 1 { "branch" } else { "branches" };
    format!(
        "Git {noun} waiting for Heddle import: {}",
        crate::cli::render::preview_list(branches, total)
    )
}

/// Execute bridge subcommands.
pub fn cmd_bridge_git(cli: &Cli, command: GitCommands) -> Result<()> {
    if matches!(&command, GitCommands::Status) {
        let cwd = std::env::current_dir()?;
        let start = cli.repo.as_ref().unwrap_or(&cwd);
        if let Some(probe) = build_plain_git_verification_probe(start)? {
            let import_hint = probe
                .import_hint
                .clone()
                .map(|hint| BridgeGitImportHintOutput {
                    current_branch: hint.current_branch,
                    missing_branch_count: hint.missing_branch_count,
                    missing_branches: hint.missing_branches,
                    recommended_command: hint.recommended_command,
                });
            let output = BridgeGitStatusOutput {
                output_kind: "bridge_git_status",
                repository_capability: "plain-git".to_string(),
                storage_model: "git-only".to_string(),
                mirror_path: None,
                mirror_initialized: false,
                git_overlay_import_hint: import_hint,
                git_overlay_health: GitOverlayHealth {
                    status: probe.trust.status.clone(),
                    clean: probe.trust.verified,
                    summary: probe.trust.summary.clone(),
                    recovery_commands: probe.trust.recovery_commands.clone(),
                    checks: probe
                        .trust
                        .checks
                        .iter()
                        .map(|check| GitOverlayHealthCheck {
                            name: check.name.clone(),
                            status: check.status.clone(),
                            summary: check.summary.clone(),
                            details: check.details.clone(),
                        })
                        .collect(),
                },
                recommended_action: probe.trust.recommended_action.clone(),
                recommended_action_argv: probe.trust.recommended_action_argv.clone(),
                recommended_action_template: probe.trust.recommended_action_template.clone(),
                recovery_commands: probe.trust.recovery_commands.clone(),
                recovery_command_argv: probe.trust.recovery_command_argv.clone(),
                trust: probe.trust,
            };
            render_bridge_git_status(&output, should_output_json(cli, None));
            return Ok(());
        }
    }

    let repo = match &cli.repo {
        Some(path) => Repository::open(path)?,
        None => Repository::open(std::env::current_dir()?)?,
    };

    let mut bridge = GitBridge::new(&repo);

    match command {
        GitCommands::Status => {
            cmd_bridge_git_status(cli, &repo)?;
        }

        GitCommands::Init { path } => {
            // Until the bridge gains a persisted mirror-location config, the
            // mirror always lives at `.heddle/git/`. Reject `--path` outright
            // rather than silently dropping it on the floor (which is what
            // the old handler did, hiding the misuse).
            if path.is_some() {
                return Err(anyhow!(
                    "--path is not yet supported for `bridge init`; the bridge \
                     mirror is always at .heddle/git. To export to a different \
                     location, use `bridge export --destination PATH`."
                ));
            }
            bridge.init_mirror()?;

            if should_output_json(cli, Some(repo.config())) {
                let out = serde_json::json!({
                    "initialized": true,
                    "path": bridge.mirror_path().display().to_string(),
                });
                println!("{out}");
            } else {
                println!(
                    "Initialized Git mirror at: {}",
                    bridge.mirror_path().display()
                );
            }
        }

        GitCommands::Export { destination } => {
            let destination = destination.ok_or_else(|| {
                anyhow!(
                    "no destination specified. Use `--destination PATH` to write a bare \
                     git repository, or `bridge push <remote>` to push to a \
                     configured remote."
                )
            })?;
            let stats = bridge.export_to_path(&destination)?;

            if should_output_json(cli, Some(repo.config())) {
                let out = serde_json::json!({
                    "states_exported": stats.states_exported,
                    "threads_synced": stats.threads_synced,
                    "markers_synced": stats.markers_synced,
                    "destination": destination.display().to_string(),
                });
                println!("{out}");
            } else {
                println!(
                    "{} exported {} to {}",
                    style::ok_marker(),
                    style::count(stats.states_exported, "state"),
                    style::dim(&destination.display().to_string())
                );
                println!(
                    "  {}",
                    style::field(
                        "threads",
                        &format!(
                            "{} synced to branches",
                            style::bold(&stats.threads_synced.to_string())
                        )
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "markers",
                        &format!(
                            "{} synced to tags",
                            style::bold(&stats.markers_synced.to_string())
                        )
                    )
                );
            }
        }

        GitCommands::Import { path, refs } => {
            let resolved = match path {
                Some(source) => Some(resolve_source(&repo, source)?),
                None => None,
            };
            let default_source = repo.root();
            let source_label = resolved
                .as_ref()
                .map(|source| source.path().display().to_string())
                .unwrap_or_else(|| default_source.display().to_string());
            let scope = if refs.is_empty() {
                "all refs".to_string()
            } else {
                format!("{} ref(s): {}", refs.len(), refs.join(", "))
            };
            let mut progress = ImportProgress::start(cli, &repo, &scope, &source_label);
            progress.advance("importing commits");
            let stats = match &resolved {
                Some(r) if refs.is_empty() => import_all(&mut bridge, Some(r.path()))?,
                Some(r) => import_selected_refs(&mut bridge, Some(r.path()), &refs)?,
                None if refs.is_empty() => import_all(&mut bridge, Some(default_source))?,
                None => import_selected_refs(&mut bridge, Some(default_source), &refs)?,
            };
            progress.advance("writing refs");
            progress.finish();

            let already_in_sync = stats.states_created == 0 && stats.commits_imported > 0;
            let trust = build_repository_verification_state(&repo);
            let summary = if already_in_sync {
                if trust.verified {
                    format!(
                        "Git import already in sync with {source_label}: every commit already imported; repository verification is clean"
                    )
                } else {
                    format!(
                        "Git import already in sync with {source_label}: every commit already imported, but repository verification is blocked: {}",
                        trust.summary
                    )
                }
            } else if trust.verified {
                format!(
                    "Imported Git history from {source_label}; repository verification is clean"
                )
            } else {
                format!(
                    "Imported Git history from {source_label}, but repository verification is blocked: {}",
                    trust.summary
                )
            };
            let output = BridgeGitImportOutput {
                output_kind: "bridge_git_import",
                status: if trust.verified {
                    "completed".to_string()
                } else {
                    "blocked".to_string()
                },
                action: "bridge git import",
                summary,
                commits_imported: stats.commits_imported,
                states_created: stats.states_created,
                branches_synced: stats.branches_synced,
                tags_synced: stats.tags_synced,
                skipped_non_commit_refs: stats.skipped_non_commit_refs.len(),
                partial_mirror_refs: stats.partial_mirror_refs.len(),
                already_in_sync,
                recommended_action: trust.recommended_action.clone(),
                recommended_action_argv: trust.recommended_action_argv.clone(),
                recommended_action_template: trust.recommended_action_template.clone(),
                recovery_commands: trust.recovery_commands.clone(),
                recovery_command_argv: trust.recovery_command_argv.clone(),
                trust,
            };
            render_bridge_git_import(cli, &repo, &output)?;
        }

        GitCommands::Sync { path } => {
            let resolved = match path {
                Some(source) => Some(resolve_source(&repo, source)?),
                None => None,
            };

            // First export Heddle states to Git
            let export_stats = export_all(&mut bridge)?;

            // Then import any new Git commits
            let import_stats = match &resolved {
                Some(r) => import_all(&mut bridge, Some(r.path()))?,
                None => import_all(&mut bridge, None)?,
            };

            // sync's `commits_imported` keeps the historical "newly
            // imported commits" meaning. After heddle#147, the import
            // walker's `commits_imported` counts every commit it
            // visited (mirroring `bridge git ingest` so a re-import
            // doesn't read 0). That would make a no-op sync of an
            // already-synced overlay look like it pulled the whole
            // history again — exactly the operator signal sync is
            // there to provide. Use `states_created` instead: it is
            // the count of commits that produced a new heddle state
            // on this sync, which is what callers reading
            // `commits_imported` from a sync result actually want.
            let sync_commits_imported = import_stats.states_created;
            let threads_synced = export_stats.threads_synced + import_stats.branches_synced;
            let markers_synced = export_stats.markers_synced + import_stats.tags_synced;
            let trust = build_repository_verification_state(&repo);
            let sync_output = BridgeGitSyncOutput {
                output_kind: "bridge_git_sync",
                status: if trust.verified {
                    "completed".to_string()
                } else {
                    "blocked".to_string()
                },
                action: "bridge git sync",
                summary: if trust.verified {
                    "Synced Git overlay; repository verification is clean".to_string()
                } else {
                    format!(
                        "Synced Git overlay, but repository verification is blocked: {}",
                        trust.summary
                    )
                },
                states_exported: export_stats.states_exported,
                commits_imported: sync_commits_imported,
                threads_synced,
                markers_synced,
                recommended_action: trust.recommended_action.clone(),
                recommended_action_argv: trust.recommended_action_argv.clone(),
                recommended_action_template: trust.recommended_action_template.clone(),
                recovery_commands: trust.recovery_commands.clone(),
                recovery_command_argv: trust.recovery_command_argv.clone(),
                trust,
            };
            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&sync_output)?);
            } else {
                println!("{} synced Git overlay", style::ok_marker());
                println!(
                    "  {}",
                    style::field(
                        "exported",
                        &style::count(sync_output.states_exported, "state")
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "imported",
                        &style::count(sync_output.commits_imported, "commit")
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "threads",
                        &format!(
                            "{} synced with branches",
                            style::bold(&sync_output.threads_synced.to_string())
                        )
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "markers",
                        &format!(
                            "{} synced with tags",
                            style::bold(&sync_output.markers_synced.to_string())
                        )
                    )
                );
                if !sync_output.trust.verified {
                    println!();
                    println!("{}", style::section("Verification"));
                    println!(
                        "  {}",
                        style::field("status", &style::thread_state(&sync_output.trust.status))
                    );
                    println!("  {}", sync_output.trust.summary);
                }
                if !sync_output.recommended_action.is_empty() {
                    println!();
                    print_next(&sync_output.recommended_action);
                }
            }
        }

        GitCommands::Reconcile {
            prefer,
            ref_name,
            preview,
        } => {
            let heddle_preview =
                canonical_bridge_reconcile_ref_preview_command(Some("heddle"), &ref_name);
            let git_preview =
                canonical_bridge_reconcile_ref_preview_command(Some("git"), &ref_name);
            let recovery_commands = match prefer.as_deref() {
                Some("git") => vec![canonical_bridge_import_ref_command(&ref_name)],
                Some("heddle") => vec![canonical_bridge_reconcile_ref_command("heddle", &ref_name)],
                None if preview => vec![heddle_preview.clone(), git_preview.clone()],
                None => return Err(anyhow!(reconcile_direction_required_advice(&ref_name))),
                _ => unreachable!("clap restricts --prefer values"),
            };
            if !preview {
                let prefer = prefer
                    .as_deref()
                    .ok_or_else(|| reconcile_direction_required_advice(&ref_name))?;
                match prefer {
                    "git" => {
                        let stats = import_selected_refs(
                            &mut bridge,
                            Some(repo.root()),
                            std::slice::from_ref(&ref_name),
                        )?;
                        if repo.git_overlay_current_branch()?.as_deref() == Some(ref_name.as_str())
                        {
                            repo.refs().write_head(&Head::Attached {
                                thread: ref_name.clone(),
                            })?;
                        }
                        let trust = build_repository_verification_state(&repo);
                        let output = BridgeGitReconcileOutput {
                            output_kind: "bridge_git_reconcile",
                            status: if trust.verified {
                                "completed".to_string()
                            } else {
                                "blocked".to_string()
                            },
                            action: "bridge git reconcile",
                            prefer: Some(prefer.to_string()),
                            ref_name: ref_name.clone(),
                            preview,
                            summary: format!(
                                "Reconciled '{ref_name}' by importing {} Git commit(s) into Heddle",
                                stats.commits_imported
                            ),
                            recommended_action: (!trust.recommended_action.is_empty())
                                .then(|| trust.recommended_action.clone()),
                            recommended_action_argv: trust.recommended_action_argv.clone(),
                            recommended_action_template: trust.recommended_action_template.clone(),
                            recovery_commands: trust.recovery_commands.clone(),
                            recovery_command_argv: trust.recovery_command_argv.clone(),
                            trust,
                        };
                        render_bridge_git_reconcile(cli, &repo, &output)?;
                        return Ok(());
                    }
                    "heddle" => {
                        let state = repo
                            .refs()
                            .get_thread(&ref_name)?
                            .ok_or_else(|| reconcile_missing_heddle_thread_advice(&ref_name))?;
                        repo.goto_without_record(&state)?;
                        repo.refs().write_head(&Head::Attached {
                            thread: ref_name.clone(),
                        })?;
                        match bridge.write_through_current_checkout()? {
                            crate::bridge::WriteThroughOutcome::Wrote(git_oid) => {
                                let trust = build_repository_verification_state(&repo);
                                let output = BridgeGitReconcileOutput {
                                    output_kind: "bridge_git_reconcile",
                                    status: if trust.verified {
                                        "completed".to_string()
                                    } else {
                                        "blocked".to_string()
                                    },
                                    action: "bridge git reconcile",
                                    prefer: Some(prefer.to_string()),
                                    ref_name: ref_name.clone(),
                                    preview,
                                    summary: format!(
                                        "Reconciled '{ref_name}' by writing Heddle state {} to Git commit {}",
                                        state.short(),
                                        git_oid
                                    ),
                                    recommended_action: (!trust.recommended_action.is_empty())
                                        .then(|| trust.recommended_action.clone()),
                                    recommended_action_argv: trust.recommended_action_argv.clone(),
                                    recommended_action_template: trust
                                        .recommended_action_template
                                        .clone(),
                                    recovery_commands: trust.recovery_commands.clone(),
                                    recovery_command_argv: trust.recovery_command_argv.clone(),
                                    trust,
                                };
                                render_bridge_git_reconcile(cli, &repo, &output)?;
                                return Ok(());
                            }
                            crate::bridge::WriteThroughOutcome::Skipped(reason) => {
                                return Err(anyhow!(reconcile_write_through_skipped_advice(
                                    &ref_name,
                                    reason.to_string(),
                                )));
                            }
                        }
                    }
                    _ => unreachable!("clap restricts --prefer values"),
                }
            }
            let trust = build_repository_verification_state(&repo);
            let output = BridgeGitReconcileOutput {
                output_kind: "bridge_git_reconcile",
                status: "preview".to_string(),
                action: "bridge git reconcile",
                prefer: prefer.clone(),
                ref_name: ref_name.clone(),
                preview,
                summary: reconcile_preview_summary(&ref_name, prefer.as_deref()),
                recommended_action: prefer
                    .as_ref()
                    .and_then(|_| recovery_commands.first().cloned()),
                recommended_action_argv: prefer.as_ref().and_then(|_| {
                    recovery_commands
                        .first()
                        .and_then(|action| action_argv(action))
                }),
                recommended_action_template: prefer.as_ref().and_then(|_| {
                    recovery_commands
                        .first()
                        .and_then(|action| action_template(action))
                }),
                recovery_command_argv: recovery_commands
                    .iter()
                    .filter_map(|action| action_argv(action))
                    .collect(),
                recovery_commands,
                trust,
            };
            render_bridge_git_reconcile(cli, &repo, &output)?;
        }

        GitCommands::Push { remote } => {
            let remote_name = resolve_default_remote_name(&repo, remote.as_deref())?;
            bridge.push(&remote_name)?;
            let trust = build_repository_verification_state(&repo);

            if should_output_json(cli, Some(repo.config())) {
                let output = bridge_git_push_output(remote_name, trust);
                crate::cli::render::write_json_stdout(&output)?;
            } else {
                println!(
                    "{} pushed to remote {}",
                    style::ok_marker(),
                    style::bold(&remote_name)
                );
                println!(
                    "Verification: {}",
                    if trust.verified {
                        style::accent(&trust.summary)
                    } else {
                        style::warn(&trust.summary)
                    }
                );
            }
        }

        GitCommands::Pull { remote } => {
            let remote_name = resolve_default_remote_name(&repo, remote.as_deref())?;
            let outcome = bridge.pull(&remote_name)?;
            let trust = build_repository_verification_state(&repo);

            if should_output_json(cli, Some(repo.config())) {
                let output = bridge_git_pull_output(remote_name, outcome.changed, trust);
                crate::cli::render::write_json_stdout(&output)?;
            } else {
                if outcome.changed {
                    println!(
                        "{} pulled from remote {}",
                        style::ok_marker(),
                        style::bold(&remote_name)
                    );
                } else {
                    println!(
                        "{} remote {} made no pull changes",
                        style::ok_marker(),
                        style::bold(&remote_name)
                    );
                }
                println!(
                    "Verification: {}",
                    if trust.verified {
                        style::accent(&trust.summary)
                    } else {
                        style::warn(&trust.summary)
                    }
                );
            }
        }

        #[cfg(feature = "ingest")]
        GitCommands::Ingest { path } => {
            let resolved = resolve_source(&repo, path)?;
            run_ingest(cli, &repo, resolved.path())?;
        }

        #[cfg(feature = "ingest")]
        GitCommands::Reason {
            path,
            max_sessions_per_commit,
            min_match_confidence,
            limit,
            claude_home,
            codex_home,
            opencode_home,
            dry_run,
        } => run_reason(
            cli,
            &repo,
            &path,
            max_sessions_per_commit,
            min_match_confidence,
            limit,
            claude_home,
            codex_home,
            opencode_home,
            dry_run,
        )?,
    }

    Ok(())
}

fn reconcile_missing_heddle_thread_advice(ref_name: &str) -> RecoveryAdvice {
    let import_command = canonical_adopt_ref_command(ref_name);
    let reconcile_git_command = canonical_bridge_reconcile_ref_command("git", ref_name);

    RecoveryAdvice::safety_refusal(
        "reconcile_missing_heddle_thread",
        format!("Cannot prefer Heddle for '{ref_name}': no matching Heddle thread exists"),
        format!(
            "Import the Git ref with `{import_command}`, or reconcile by preferring Git with `{reconcile_git_command}`."
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

fn reconcile_direction_required_advice(ref_name: &str) -> RecoveryAdvice {
    let preview_command = canonical_bridge_reconcile_ref_preview_command(None, ref_name);
    RecoveryAdvice::safety_refusal(
        "reconcile_direction_required",
        format!("Refusing to reconcile '{ref_name}': choose a local side before applying"),
        format!(
            "Run `{preview_command}` to inspect both local repair choices, then rerun with `--prefer heddle` or `--prefer git`."
        ),
        "no --prefer side was supplied for a non-preview reconcile",
        "applying reconcile without a side would need to choose whether Heddle or the local Git branch is authoritative",
        "Git refs, Heddle refs, index, remotes, and worktree files were left unchanged",
        preview_command.clone(),
        vec![
            preview_command,
            canonical_bridge_reconcile_ref_preview_command(Some("heddle"), ref_name),
            canonical_bridge_reconcile_ref_preview_command(Some("git"), ref_name),
        ],
    )
}

fn reconcile_preview_summary(ref_name: &str, prefer: Option<&str>) -> String {
    match prefer {
        Some("heddle") => format!(
            "Preview: prefer Heddle for '{ref_name}'. Apply would write the Heddle thread state into local Git and update the checkout; no refs, remotes, index, or worktree files were changed in preview"
        ),
        Some("git") => format!(
            "Preview: prefer Git for '{ref_name}'. Apply would import the local Git branch tip into Heddle and leave local Git authoritative; no refs, remotes, index, or worktree files were changed in preview"
        ),
        None => format!(
            "Preview: local Git/Heddle repair choices for '{ref_name}'. Choose --prefer heddle to write the Heddle thread state into local Git, or --prefer git to import the local Git branch tip into Heddle. This does not push, pull, rewrite remotes, move refs, update the index, or change worktree files"
        ),
        _ => unreachable!("clap restricts --prefer values"),
    }
}

fn reconcile_write_through_skipped_advice(ref_name: &str, reason: String) -> RecoveryAdvice {
    let preview_command = canonical_bridge_reconcile_ref_preview_command(Some("heddle"), ref_name);

    RecoveryAdvice::safety_refusal(
        "reconcile_write_through_skipped",
        format!("Could not reconcile '{ref_name}' by preferring Heddle: {reason}"),
        format!("Inspect the reconcile plan with `{preview_command}` before retrying."),
        reason,
        "writing the Heddle state into Git could not be completed for the active checkout",
        "Heddle state was preserved; Git write-through did not report a new commit",
        preview_command.clone(),
        vec![preview_command],
    )
}

#[cfg(feature = "ingest")]
fn run_ingest(cli: &Cli, repo: &Repository, git_path: &std::path::Path) -> Result<()> {
    use ingest::import_git_into;

    let (stats, _map) = import_git_into(git_path, repo.root())?;
    if should_output_json(cli, Some(repo.config())) {
        let out = serde_json::json!({
            "commits_imported": stats.commits_imported,
            "trees_imported": stats.trees_imported,
            "blobs_imported": stats.blobs_imported,
            "local_branches": stats.refs_seen.local_branches,
            "tags": stats.refs_seen.tags,
            "remote_branches": stats.refs_seen.remote_branches,
            "symbolic_skipped": stats.refs_seen.symbolic_skipped,
            "peel_failed": stats.refs_seen.peel_failed,
            "non_commit_skipped": stats.refs_seen.non_commit_skipped,
            "reflog_only_commits": stats.reflog_only_commits,
        });
        println!("{out}");
    } else {
        let r = &stats.refs_seen;
        let walked = r.local_branches
            + r.tags
            + r.remote_branches
            + r.symbolic_skipped
            + r.peel_failed
            + r.non_commit_skipped;
        let kept = r.local_branches + r.tags + r.remote_branches;
        let ignored = r.symbolic_skipped + r.peel_failed + r.non_commit_skipped;
        println!("imported from {}", git_path.display());
        println!("refs:  walked: {walked}  kept: {kept}  ignored: {ignored}");
        println!("    local branches:  {}", r.local_branches);
        println!("    tags:            {}", r.tags);
        println!("    remote branches: {}", r.remote_branches);
        if r.symbolic_skipped > 0 {
            println!("    symbolic refs:   {} ignored", r.symbolic_skipped);
        }
        if r.peel_failed > 0 {
            println!("    peel-failed:     {} ignored", r.peel_failed);
        }
        if r.non_commit_skipped > 0 {
            println!(
                "    non-commit refs: {} ignored (annotated tag → blob/tree)",
                r.non_commit_skipped
            );
        }
        println!("commits:  imported: {}", stats.commits_imported);
        if stats.reflog_only_commits > 0 {
            println!("    reflog-only:   {}", stats.reflog_only_commits);
        }
        println!("trees:    {}", stats.trees_imported);
        println!("blobs:    {}", stats.blobs_imported);
        println!("threads written:  {}", stats.refs.threads_written);
        println!("markers written:  {}", stats.refs.markers_written);
        println!();
        println!(
            "Next: `heddle bridge reason --path {}` to attach AI-session reasoning.",
            git_path.display()
        );
    }
    Ok(())
}

#[cfg(feature = "ingest")]
#[allow(clippy::too_many_arguments)]
fn run_reason(
    cli: &Cli,
    repo: &Repository,
    git_path: &std::path::Path,
    max_sessions_per_commit: usize,
    min_match_confidence: f32,
    limit: Option<usize>,
    claude_home: Option<String>,
    codex_home: Option<String>,
    opencode_home: Option<String>,
    dry_run: bool,
) -> Result<()> {
    use std::path::PathBuf;

    use ingest::{
        GitSource, ReasoningPipeline, ReasoningPipelineParams, ShaMap, TranscriptRoots,
        load_transcripts, pipeline_default_commits,
    };

    let map_path = repo.heddle_dir().join("ingest").join("sha_map.sqlite");
    if !map_path.exists() {
        anyhow::bail!(RecoveryAdvice::bridge_ingest_required(
            &map_path.display().to_string(),
            &git_path.display().to_string(),
        ));
    }
    let map = ShaMap::open(&map_path)?;
    let git = GitSource::open(git_path)?;

    let resolve = |ovr: Option<String>, fallback: Option<PathBuf>| -> Option<PathBuf> {
        match ovr.as_deref() {
            Some("") => None,
            Some(s) => Some(PathBuf::from(s)),
            None => fallback,
        }
    };
    let default = TranscriptRoots::default();
    let roots = TranscriptRoots {
        claude: resolve(claude_home, default.claude),
        codex: resolve(codex_home, default.codex),
        opencode_home: resolve(opencode_home, default.opencode_home),
        codex_since: None,
    };
    let transcripts = load_transcripts(git_path, &roots);
    println!("loaded {} transcripts", transcripts.len());

    let mut commits = pipeline_default_commits(&map);
    if let Some(n) = limit {
        commits.truncate(n);
    }
    println!("processing {} commits", commits.len());

    let mut params = ReasoningPipelineParams {
        max_sessions_per_commit,
        min_match_confidence,
        ..ReasoningPipelineParams::default()
    };
    if dry_run {
        params.emit_annotations = false;
    }
    let mut pipeline =
        ReasoningPipeline::new(repo, &git, &map, git_path, transcripts).with_params(params);
    let stats = pipeline.run(&commits)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{{\"commits_scanned\":{},\"commits_with_matches\":{},\"sessions_mined\":{},\
             \"points_extracted\":{},\"states_updated\":{},\"annotations_written\":{}}}",
            stats.commits_scanned,
            stats.commits_with_matches,
            stats.sessions_mined,
            stats.points_extracted,
            stats.emit.states_updated,
            stats.emit.annotations_written,
        );
    } else {
        if dry_run {
            println!("dry-run: not writing annotations");
        }
        println!("commits scanned:      {}", stats.commits_scanned);
        println!("commits with matches: {}", stats.commits_with_matches);
        println!("sessions mined:       {}", stats.sessions_mined);
        println!("points extracted:     {}", stats.points_extracted);
        println!("points rejected:      {}", stats.points_rejected_quality);
        println!("states updated:       {}", stats.emit.states_updated);
        println!("annotations written:  {}", stats.emit.annotations_written);
        if dry_run && !pipeline.preview().is_empty() {
            println!();
            println!("candidate preview:");
            for item in pipeline.preview() {
                let decision = match item.decision {
                    ingest::reasoning_pipeline::PreviewDecision::Kept => "kept",
                    ingest::reasoning_pipeline::PreviewDecision::Rejected => "rejected",
                };
                println!(
                    "- {decision} ({}) {} {}",
                    item.reason,
                    &item.commit_sha[..item.commit_sha.len().min(8)],
                    item.target_file
                );
                println!("  {}", item.text);
            }
        }
        if stats.emit.states_updated > 0 {
            println!();
            println!(
                "annotations attached to {} states. Browse them with:",
                stats.emit.states_updated
            );
            println!("  heddle log              # find a state id");
            println!("  heddle context list --ref <state-id>");
            println!("or open `/app/repo/<repo>/-/files/<path>` to see them inline.");
        }
    }
    Ok(())
}
