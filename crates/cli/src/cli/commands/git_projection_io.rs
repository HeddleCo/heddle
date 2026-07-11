// SPDX-License-Identifier: Apache-2.0
//! Git projection import/export/sync command implementations.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use heddle_core::git_projection_io_plan::{
    ExportedRefSummaryFact, export_commits_summary, exported_refs_summary,
};
use ingest::{ImportOptions, LossyImportEntry};
use objects::object::{ChangeId, ThreadName};
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    import_progress::ImportProgress,
    next_action::{NextActionValidationContext, write_full_command_json},
    verification_health::{
        RepositoryVerificationState, build_repository_verification_state,
        serialize_empty_action_as_null,
    },
};
use crate::{
    cli::{Cli, cli_args::GitSource, should_output_json, style},
    git_projection_engine::{
        GitProjection, git_core::clone_url_to_bare, git_export::export_all,
        git_ingest::import_git_history, git_util::ExportedRef,
    },
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

#[derive(Serialize)]
struct GitProjectionImportOutput {
    output_kind: &'static str,
    status: String,
    action: &'static str,
    summary: String,
    commits_imported: usize,
    states_created: usize,
    branches_synced: usize,
    tags_synced: usize,
    skipped_non_commit_refs: usize,
    lossy_entries: Vec<GitProjectionLossyImportEntryOutput>,
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
struct GitProjectionLossyImportEntryOutput {
    path: String,
    action: String,
    reason: String,
    git_object: Option<String>,
}

#[derive(Serialize)]
struct GitProjectionSyncOutput {
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

/// Render a `branches:`/`tags:` line from exported refs (plain text core plan).
fn exported_refs_summary_for_cli(refs: &[ExportedRef]) -> String {
    // `ExportedRefSummaryFact` borrows tip hex; materialize tip strings first.
    let tip_hexes: Vec<String> = refs.iter().map(|r| r.tip.to_hex()).collect();
    let facts: Vec<ExportedRefSummaryFact<'_>> = refs
        .iter()
        .zip(tip_hexes.iter())
        .map(|(r, tip)| ExportedRefSummaryFact {
            name: r.name.as_str(),
            tip_hex: tip.as_str(),
        })
        .collect();
    exported_refs_summary(&facts)
}

/// JSON projection of exported refs: `[{"name":..,"tip":<full sha>}]`.
fn exported_refs_json(refs: &[ExportedRef]) -> serde_json::Value {
    serde_json::Value::Array(
        refs.iter()
            .map(|r| serde_json::json!({ "name": r.name, "tip": r.tip.to_string() }))
            .collect(),
    )
}

fn render_import_git(
    cli: &Cli,
    repo: &Repository,
    output: &GitProjectionImportOutput,
    command_path: &[&str],
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            output,
            NextActionValidationContext::new(command_path, repo.capability()),
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

fn git_projection_lossy_import_entries(
    entries: &[LossyImportEntry],
) -> Vec<GitProjectionLossyImportEntryOutput> {
    entries
        .iter()
        .map(|entry| GitProjectionLossyImportEntryOutput {
            path: entry.path.clone(),
            action: entry.action.as_str().to_string(),
            reason: entry.reason.clone(),
            git_object: entry.git_object.clone(),
        })
        .collect()
}

/// Execute Git projection subcommands.
fn open_repo_for_cli(cli: &Cli) -> Result<Repository> {
    match &cli.repo {
        Some(path) => Ok(Repository::open(path)?),
        None => Ok(Repository::open(std::env::current_dir()?)?),
    }
}

pub fn cmd_export_git(cli: &Cli, destination: Option<PathBuf>) -> Result<()> {
    let repo = open_repo_for_cli(cli)?;
    let mut bridge = GitProjection::new(&repo);
    run_git_export(
        cli,
        &repo,
        &mut bridge,
        destination,
        "no destination specified. Use `export git --destination PATH` to write a bare Git repository, or `push <remote>` to push to a configured remote.",
    )
}

pub fn cmd_import_git(
    cli: &Cli,
    path: Option<GitSource>,
    refs: Vec<String>,
    lossy: bool,
) -> Result<()> {
    let repo = open_repo_for_cli(cli)?;
    let mut bridge = GitProjection::new(&repo);
    run_git_import(
        cli,
        &repo,
        &mut bridge,
        path,
        &refs,
        lossy,
        "import_git",
        "import git",
        &["import", "git"],
    )
}

pub fn cmd_sync_git(cli: &Cli, path: Option<GitSource>) -> Result<()> {
    let repo = open_repo_for_cli(cli)?;
    let mut bridge = GitProjection::new(&repo);
    run_git_sync(
        cli,
        &repo,
        &mut bridge,
        path,
        "sync_git",
        "sync git",
        &["sync", "git"],
    )
}

fn run_git_export(
    cli: &Cli,
    repo: &Repository,
    bridge: &mut GitProjection,
    destination: Option<PathBuf>,
    missing_destination_message: &'static str,
) -> Result<()> {
    let destination = destination.ok_or_else(|| anyhow!(missing_destination_message))?;
    let stats = bridge.export_to_path(&destination)?;

    if should_output_json(cli, Some(repo.config())) {
        let out = serde_json::json!({
            "output_kind": "export_git",
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
            style::field("branches", &exported_refs_summary_for_cli(&stats.branches))
        );
        println!(
            "  {}",
            style::field("tags", &exported_refs_summary_for_cli(&stats.tags))
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_git_import(
    cli: &Cli,
    repo: &Repository,
    bridge: &mut GitProjection,
    path: Option<GitSource>,
    refs: &[String],
    lossy: bool,
    output_kind: &'static str,
    action: &'static str,
    command_path: &[&str],
) -> Result<()> {
    let resolved = match path.as_ref() {
        Some(source) => Some(resolve_source(repo, source.clone())?),
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
    let mut progress = ImportProgress::start(cli, repo, &scope, &source_label);
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
        bridge,
        Some(source_path),
        refs,
        import_options,
        Some(&mut on_commit),
    )?;
    progress.begin_ref_write();
    progress.finish();
    materialize_imported_attached_thread(bridge, attached_before.as_ref())?;

    let already_in_sync = stats.states_created == 0 && stats.commits_imported > 0;
    let trust = build_repository_verification_state(repo);
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
        format!("Imported Git history from {source_label}; repository verification is clean")
    } else {
        format!(
            "Imported Git history from {source_label}, but repository verification is blocked: {}",
            trust.summary
        )
    };
    let output = GitProjectionImportOutput {
        output_kind,
        status: if trust.verified {
            "completed".to_string()
        } else {
            "blocked".to_string()
        },
        action,
        summary,
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.branches_synced,
        tags_synced: stats.tags_synced,
        skipped_non_commit_refs: stats.skipped_non_commit_refs,
        lossy_entries: git_projection_lossy_import_entries(&stats.lossy_entries),
        already_in_sync,
        recommended_action: trust.recommended_action.clone(),
        recommended_action_template: trust.recommended_action_template.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        trust,
    };
    render_import_git(cli, repo, &output, command_path)
}

fn run_git_sync(
    cli: &Cli,
    repo: &Repository,
    bridge: &mut GitProjection,
    path: Option<GitSource>,
    output_kind: &'static str,
    action: &'static str,
    command_path: &[&str],
) -> Result<()> {
    let resolved = match path {
        Some(source) => Some(resolve_source(repo, source)?),
        None => None,
    };

    let export_stats = export_all(bridge)?;
    let import_stats = match &resolved {
        Some(source) => import_git_history(
            bridge,
            Some(source.path()),
            &[],
            ImportOptions::default(),
            None,
        )?,
        None => import_git_history(
            bridge,
            Some(repo.root()),
            &[],
            ImportOptions::default(),
            None,
        )?,
    };

    // Sync reports commits that produced new Heddle states on this run,
    // not every Git commit walked by the importer.
    let sync_commits_imported = import_stats.states_created;
    let threads_synced = export_stats.threads_synced + import_stats.branches_synced;
    let markers_synced = export_stats.markers_synced + import_stats.tags_synced;
    let trust = build_repository_verification_state(repo);
    let sync_output = GitProjectionSyncOutput {
        output_kind,
        status: if trust.verified {
            "completed".to_string()
        } else {
            "blocked".to_string()
        },
        action,
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
            NextActionValidationContext::new(command_path, repo.capability()),
        )?;
    } else {
        println!("{} synced Git overlay", style::ok_marker());
        println!(
            "  {}",
            style::field(
                "exported",
                &export_commits_summary(
                    sync_output.commits_exported_total,
                    sync_output.states_exported,
                ),
            ),
        );
        println!(
            "  {}",
            style::field(
                "imported",
                &style::count(sync_output.commits_imported, "commit"),
            ),
        );
        println!(
            "  {}",
            style::field(
                "threads",
                &format!(
                    "{} synced with branches",
                    style::bold(&sync_output.threads_synced.to_string()),
                ),
            ),
        );
        println!(
            "  {}",
            style::field(
                "markers",
                &format!(
                    "{} synced with tags",
                    style::bold(&sync_output.markers_synced.to_string()),
                ),
            ),
        );
        if !sync_output.trust.verified {
            println!();
            println!("{}", style::section("Verification"));
            println!(
                "  {}",
                style::field("status", &style::thread_state(&sync_output.trust.status)),
            );
            println!("  {}", sync_output.trust.summary);
        }
        if !sync_output.recommended_action.is_empty() {
            println!();
            print_next(&sync_output.recommended_action);
        }
    }
    Ok(())
}

#[cfg(feature = "ingest")]
#[allow(clippy::too_many_arguments)]
pub fn cmd_context_reason_git(
    cli: &Cli,
    path: &Path,
    max_sessions_per_commit: usize,
    min_match_confidence: f32,
    limit: Option<usize>,
    claude_home: Option<String>,
    codex_home: Option<String>,
    opencode_home: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let repo = open_repo_for_cli(cli)?;
    run_reason(
        cli,
        &repo,
        path,
        max_sessions_per_commit,
        min_match_confidence,
        limit,
        claude_home,
        codex_home,
        opencode_home,
        dry_run,
    )
}

fn materialize_imported_attached_thread(
    bridge: &mut GitProjection<'_>,
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
        anyhow::bail!(RecoveryAdvice::git_import_metadata_required(
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
