// SPDX-License-Identifier: Apache-2.0
//! Bridge command implementations.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use ingest::{ImportOptions, LossyImportEntry};
use objects::object::{ChangeId, ThreadName};
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::{
    action_line::{print_next, print_next_step, print_optional},
    advice::RecoveryAdvice,
    git_overlay_health::{
        GitOverlayHealth, GitOverlayHealthCheck, RepositoryVerificationState, action_template,
        build_git_overlay_health, build_plain_git_verification_probe,
        build_repository_verification_state, canonical_adopt_ref_command,
        canonical_bridge_import_ref_command, canonical_bridge_reconcile_ref_command,
        canonical_bridge_reconcile_ref_preview_command, serialize_empty_action_as_null,
    },
    import_progress::ImportProgress,
    next_action::{NextActionValidationContext, write_full_command_json},
    remote::resolve_default_remote_name,
};
use heddle_core::bridge::{
    GitBridge, git_core::clone_url_to_bare, git_export::export_all,
    git_ingest::import_git_history, git_util::ExportedRef,
};

use crate::cli::{Cli, GitCommands, cli_args::GitSource, should_output_json, style};

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
    #[allow(dead_code)]
    #[serde(skip_serializing)]
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
    #[allow(dead_code)]
    #[serde(skip_serializing)]
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
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
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
    lossy_entries: Vec<BridgeLossyImportEntryOutput>,
    already_in_sync: bool,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct BridgeLossyImportEntryOutput {
    path: String,
    action: String,
    reason: String,
    git_object: Option<String>,
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
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    #[serde(skip_serializing)]
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
    commits_exported_total: usize,
    commits_imported: usize,
    threads_synced: usize,
    markers_synced: usize,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

/// Render the `commits:` line for an export/sync summary: the total
/// commits written to the destination, broken down into newly-minted vs.
/// already-present. In the common git-overlay case `newly` is 0 and this
/// reads "N total (already in sync)" rather than a misleading bare 0.
fn export_commits_summary(total: usize, newly: usize) -> String {
    let already = total.saturating_sub(newly);
    let breakdown = if total == 0 {
        String::new()
    } else if newly == 0 {
        format!(" ({})", style::accent("already in sync"))
    } else if already == 0 {
        format!(" ({} newly written)", style::bold(&newly.to_string()))
    } else {
        format!(
            " ({} newly written, {} already in sync)",
            style::bold(&newly.to_string()),
            style::bold(&already.to_string())
        )
    };
    format!("{} total{}", style::bold(&total.to_string()), breakdown)
}

/// Render a `branches:`/`tags:` line: the count, then each ref name with
/// its tip short-SHA, e.g. `3   main af25b9d · spike-ok 7f1002c`.
fn exported_refs_summary(refs: &[ExportedRef]) -> String {
    let count = style::bold(&refs.len().to_string());
    if refs.is_empty() {
        return count;
    }
    let listing = refs
        .iter()
        .map(|r| {
            let short_tip = r.tip.to_hex().chars().take(7).collect::<String>();
            format!("{} {}", r.name, style::dim(&short_tip))
        })
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{count}   {listing}")
}

/// JSON projection of exported refs: `[{"name":..,"tip":<full sha>}]`.
fn exported_refs_json(refs: &[ExportedRef]) -> serde_json::Value {
    serde_json::Value::Array(
        refs.iter()
            .map(|r| serde_json::json!({ "name": r.name, "tip": r.tip.to_string() }))
            .collect(),
    )
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
        recommended_action_template: trust.recommended_action_template.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        trust,
    };
    render_bridge_git_status(
        &output,
        should_output_json(cli, Some(repo.config())),
        cli.verbose > 0,
        NextActionValidationContext::new(&["bridge", "git", "status"], repo.capability()),
    )?;
    Ok(())
}

fn render_bridge_git_status(
    output: &BridgeGitStatusOutput,
    json: bool,
    verbose: bool,
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    if json {
        write_full_command_json(output, context)?;
        return Ok(());
    }
    // Mode preamble is read-path noise (heddle#275); keep it under `-v`.
    if verbose {
        println!(
            "Repository: {}",
            crate::cli::render::repository_mode_label(
                &output.repository_capability,
                &output.storage_model
            )
        );
    }
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
    Ok(())
}

fn render_bridge_git_import(
    cli: &Cli,
    repo: &Repository,
    output: &BridgeGitImportOutput,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            output,
            NextActionValidationContext::new(&["bridge", "git", "import"], repo.capability()),
        )?;
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
    if !output.lossy_entries.is_empty() {
        println!(
            "{} lossy import accepted for {} tree entries",
            style::warn_marker(),
            style::bold(&output.lossy_entries.len().to_string())
        );
        for entry in &output.lossy_entries {
            let object = entry
                .git_object
                .as_deref()
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            println!(
                "  {} {}{}: {}",
                entry.action, entry.path, object, entry.reason
            );
        }
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

fn bridge_lossy_import_entries(entries: &[LossyImportEntry]) -> Vec<BridgeLossyImportEntryOutput> {
    entries
        .iter()
        .map(|entry| BridgeLossyImportEntryOutput {
            path: entry.path.clone(),
            action: entry.action.as_str().to_string(),
            reason: entry.reason.clone(),
            git_object: entry.git_object.clone(),
        })
        .collect()
}

fn render_bridge_git_reconcile(
    cli: &Cli,
    repo: &Repository,
    output: &BridgeGitReconcileOutput,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            output,
            NextActionValidationContext::new(&["bridge", "git", "reconcile"], repo.capability()),
        )?;
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
                recommended_action_template: probe.trust.recommended_action_template.clone(),
                recovery_commands: probe.trust.recovery_commands.clone(),
                trust: probe.trust,
            };
            render_bridge_git_status(
                &output,
                should_output_json(cli, None),
                cli.verbose > 0,
                NextActionValidationContext::without_repo(&["bridge", "git", "status"]),
            )?;
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
                    "commits_total": stats.commits_total,
                    "threads_synced": stats.threads_synced,
                    "markers_synced": stats.markers_synced,
                    "branches": exported_refs_json(&stats.branches),
                    "tags": exported_refs_json(&stats.tags),
                    "destination": destination.display().to_string(),
                });
                println!("{out}");
            } else {
                println!(
                    "{} exported to {}",
                    style::ok_marker(),
                    style::dim(&destination.display().to_string())
                );
                println!(
                    "  {}",
                    style::field(
                        "commits",
                        &export_commits_summary(stats.commits_total, stats.states_exported)
                    )
                );
                println!(
                    "  {}",
                    style::field("branches", &exported_refs_summary(&stats.branches))
                );
                println!(
                    "  {}",
                    style::field("tags", &exported_refs_summary(&stats.tags))
                );
            }
        }

        GitCommands::Import { path, refs, lossy } => {
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
            progress.begin_commit_import();
            let import_options = ImportOptions { lossy };
            let mut on_commit = |event| progress.commit_tick(event);
            let source_path = resolved
                .as_ref()
                .map(ResolvedSource::path)
                .unwrap_or(default_source);
            let attached_before = match repo.head_ref()? {
                Head::Attached { thread } => repo
                    .refs()
                    .get_thread(&thread)?
                    .map(|state| (thread, state)),
                Head::Detached { .. } => None,
            };
            let stats = import_git_history(
                &mut bridge,
                Some(source_path),
                &refs,
                import_options,
                Some(&mut on_commit),
            )?;
            progress.begin_ref_write();
            progress.finish();
            materialize_imported_attached_thread(&mut bridge, attached_before.as_ref())?;

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
                skipped_non_commit_refs: stats.skipped_non_commit_refs,
                lossy_entries: bridge_lossy_import_entries(&stats.lossy_entries),
                already_in_sync,
                recommended_action: trust.recommended_action.clone(),
                recommended_action_template: trust.recommended_action_template.clone(),
                recovery_commands: trust.recovery_commands.clone(),
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
                Some(r) => import_git_history(
                    &mut bridge,
                    Some(r.path()),
                    &[],
                    ImportOptions::default(),
                    None,
                )?,
                None => import_git_history(
                    &mut bridge,
                    Some(repo.root()),
                    &[],
                    ImportOptions::default(),
                    None,
                )?,
            };

            // sync's `commits_imported` keeps the historical "newly
            // imported commits" meaning. After heddle#147, the import
            // walker's `commits_imported` counts every commit it
            // visited (mirroring the ingest-backed import path so a re-import
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
                commits_exported_total: export_stats.commits_total,
                commits_imported: sync_commits_imported,
                threads_synced,
                markers_synced,
                recommended_action: trust.recommended_action.clone(),
                recommended_action_template: trust.recommended_action_template.clone(),
                recovery_commands: trust.recovery_commands.clone(),
                trust,
            };
            if should_output_json(cli, Some(repo.config())) {
                write_full_command_json(
                    &sync_output,
                    NextActionValidationContext::new(&["bridge", "git", "sync"], repo.capability()),
                )?;
            } else {
                println!("{} synced Git overlay", style::ok_marker());
                println!(
                    "  {}",
                    style::field(
                        "exported",
                        &export_commits_summary(
                            sync_output.commits_exported_total,
                            sync_output.states_exported
                        )
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
                        let stats = import_git_history(
                            &mut bridge,
                            Some(repo.root()),
                            std::slice::from_ref(&ref_name),
                            ImportOptions::default(),
                            None,
                        )?;
                        if repo.git_overlay_current_branch()?.as_deref() == Some(ref_name.as_str())
                        {
                            repo.refs().write_head(&Head::Attached {
                                thread: ThreadName::new(&ref_name),
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
                            recommended_action_template: trust.recommended_action_template.clone(),
                            recovery_commands: trust.recovery_commands.clone(),
                            trust,
                        };
                        render_bridge_git_reconcile(cli, &repo, &output)?;
                        return Ok(());
                    }
                    "heddle" => {
                        let tn = ThreadName::new(&ref_name);
                        let state = repo
                            .refs()
                            .get_thread(&tn)?
                            .ok_or_else(|| reconcile_missing_heddle_thread_advice(&ref_name))?;
                        repo.goto_without_record(&state)?;
                        repo.refs().write_head(&Head::Attached { thread: tn })?;
                        match bridge.write_through_current_checkout()? {
                            heddle_core::bridge::WriteThroughOutcome::Wrote(git_oid) => {
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
                                    recommended_action_template: trust
                                        .recommended_action_template
                                        .clone(),
                                    recovery_commands: trust.recovery_commands.clone(),
                                    trust,
                                };
                                render_bridge_git_reconcile(cli, &repo, &output)?;
                                return Ok(());
                            }
                            heddle_core::bridge::WriteThroughOutcome::Skipped(reason) => {
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
                recommended_action_template: prefer.as_ref().and_then(|_| {
                    recovery_commands
                        .first()
                        .and_then(|action| action_template(action))
                }),
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

fn materialize_imported_attached_thread(
    bridge: &mut GitBridge<'_>,
    attached_before: Option<&(ThreadName, ChangeId)>,
) -> Result<()> {
    let Some((thread, old_state)) = attached_before else {
        return Ok(());
    };
    let Some(new_state) = bridge.heddle_repo.refs().get_thread(thread)? else {
        return Ok(());
    };
    if new_state == *old_state {
        return Ok(());
    }

    bridge.heddle_repo.refs().set_thread(thread, old_state)?;
    bridge.heddle_repo.refs().write_head(&Head::Attached {
        thread: thread.clone(),
    })?;
    bridge
        .heddle_repo
        .goto_verified_clean_without_record(&new_state)?;
    bridge.heddle_repo.refs().set_thread(thread, &new_state)?;
    bridge.heddle_repo.refs().write_head(&Head::Attached {
        thread: thread.clone(),
    })?;

    if bridge.heddle_repo.root().join(".git").exists() {
        bridge.write_current_checkout_from_existing_mirror()?;
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
